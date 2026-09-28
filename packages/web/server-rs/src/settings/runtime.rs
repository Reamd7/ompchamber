//! Port of `server/lib/opencode/settings-runtime.js`: settings.json disk IO
//! (atomic write with restrictive permissions), startup migrations (legacy
//! keys, deterministic project ids, project-scoped storage moves, orphan
//! recovery), and the serialized persist pipeline.
//! （中文概要）settings.json 磁盘 IO（原子写 + 收紧权限）、启动迁移
//! （遗留键清理、确定性项目 id、项目作用域存储搬迁、孤儿恢复）与
//! 串行化的 persist 流水线。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{Map, Value};

use super::helpers::{
    SanitizedUpdate, format_settings_response, merge_persisted_settings, sanitize_settings_update,
};
use super::model::Settings;
use super::normalization::{
    create_project_id_from_path, js_truthy, normalize_directory_path,
    normalize_managed_remote_tunnel_hostname, normalize_managed_remote_tunnel_preset_tokens,
    normalize_managed_remote_tunnel_presets, normalize_settings_paths, normalize_string_array,
    path_relative, path_resolve, sanitize_projects, sha1_hex, spread_into, unique_strings,
};
use crate::error::AppError;

/// 项目图标支持的扩展名，图标迁移时逐个尝试改名。
const PROJECT_ICON_EXTENSIONS: [&str; 5] = ["png", "jpg", "svg", "webp", "ico"];

/// 缺少亮色偏好时的默认亮色主题 id。
const DEFAULT_LIGHT_THEME_ID: &str = "flexoki-light";
/// 缺少暗色偏好时的默认暗色主题 id。
const DEFAULT_DARK_THEME_ID: &str = "flexoki-dark";

/// 四类通知事件（completion/error/question/subtask）的默认模板，键为
/// 事件名，值为含 title/message 占位符字符串的对象。
fn default_notification_templates() -> Map<String, Value> {
    let mut templates = Map::new();
    let mut insert = |event: &str, title: &str, message: &str| {
        templates.insert(
            event.to_string(),
            serde_json::json!({ "title": title, "message": message }),
        );
    };
    insert(
        "completion",
        "{agent_name} is ready",
        "{model_name} completed the task",
    );
    insert("error", "Tool error", "{last_message}");
    insert("question", "Input needed", "{last_message}");
    insert(
        "subtask",
        "{agent_name} is ready",
        "{model_name} completed the task",
    );
    templates
}

/// `ensureNotificationTemplateShape` — fills missing/invalid entries and
/// reports whether anything changed.
/// 逐事件补齐缺失/非法的 title、message 字段并回落默认值，返回
/// （修复后的模板表, 是否有改动）。
fn ensure_notification_template_shape(templates: Option<&Value>) -> (Map<String, Value>, bool) {
    let input = templates.filter(|v| v.as_object().is_some());
    let mut changed = false;
    let mut next = Map::new();
    let defaults = default_notification_templates();
    for (event, base) in &defaults {
        let entry = input.and_then(|i| i.get(event));
        let title = entry
            .and_then(|e| e.get("title"))
            .and_then(Value::as_str)
            .unwrap_or(
                base.get("title")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            );
        let message = entry
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .unwrap_or(
                base.get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            );
        let entry_ok = entry
            .map(|e| {
                e.get("title").and_then(Value::as_str).is_some()
                    && e.get("message").and_then(Value::as_str).is_some()
            })
            .unwrap_or(false);
        if !entry_ok {
            changed = true;
        }
        next.insert(
            event.clone(),
            serde_json::json!({ "title": title, "message": message }),
        );
    }
    (next, changed)
}

/// 当前 Unix 毫秒时间戳；时钟异常时返回 0。
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Raw JSON file IO (settings-runtime.js readJsonFile / writeJsonFile)
// ---------------------------------------------------------------------------

/// `readJsonFile`: `None` for a missing file or non-object payload, error for
/// anything else (malformed JSON included).
/// 读 JSON 文件：文件不存在或顶层不是对象/数组返回 None；其余失败
/// （含 JSON 解析错误）返回 Err。
async fn read_json_file(path: &Path) -> Result<Option<Value>, AppError> {
    let raw = match tokio::fs::read_to_string(path).await {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let parsed: Value = serde_json::from_str(&raw).map_err(|err| {
        AppError::internal(format!("Failed to parse {}: {err}", path.to_string_lossy()))
    })?;
    match parsed {
        Value::Object(_) | Value::Array(_) => Ok(Some(parsed)),
        _ => Ok(None),
    }
}

/// `writeJsonFile`: mkdir parents + pretty-printed write (not atomic — that
/// is reserved for settings.json itself via [`SettingsStore::write_raw`]).
/// 写 JSON 文件：先创建父目录再 pretty 写入；非原子写——原子性仅保留
/// 给 settings.json 本身（见 SettingsStore::write_raw）。
async fn write_json_file(path: &Path, value: &Value) -> Result<(), AppError> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let body = serde_json::to_string_pretty(value)
        .map_err(|err| AppError::internal(format!("serialize failed: {err}")))?;
    tokio::fs::write(path, body).await?;
    Ok(())
}

/// 删除文件，文件不存在视为成功；供迁移清理使用。
async fn remove_file_force(path: &Path) -> Result<(), AppError> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

/// 递归删除目录，目录不存在视为成功。
async fn remove_dir_force(path: &Path) -> Result<(), AppError> {
    match tokio::fs::remove_dir_all(path).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

// ---------------------------------------------------------------------------
// Project-scoped storage migration (settings-runtime.js)
// ---------------------------------------------------------------------------

/// 取 JSON 值的对象形态克隆；非对象（含 null）得空 Map。
fn value_object(v: Option<&Value>) -> Map<String, Value> {
    match v {
        Some(Value::Object(map)) => map.clone(),
        _ => Map::new(),
    }
}

/// `mergeByKey`: new items first, then old, deduped by an identity key.
/// 按身份键去重合并两个条目数组：新条目在前、旧条目补后；身份键取
/// key_fields 中首个命中且非空的字符串字段，无键或非对象条目被丢弃。
fn merge_by_key(
    old_items: Option<&Vec<Value>>,
    new_items: Option<&Vec<Value>>,
    key_fields: &[&str],
) -> Vec<Value> {
    let mut result: Vec<Value> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for item in new_items
        .into_iter()
        .flatten()
        .chain(old_items.into_iter().flatten())
    {
        let Some(entry) = item.as_object() else {
            continue;
        };
        let Some(key) = key_fields.iter().find_map(|field| {
            entry
                .get(*field)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
        }) else {
            continue;
        };
        if !seen.insert(key.to_string()) {
            continue;
        }
        result.push(item.clone());
    }
    result
}

/// `remapPlanPaths`: rewrite plan file paths that live under `from_dir` to
/// live under `to_dir`.
/// 把位于 from_dir 下的 plan 文件路径改写到 to_dir 下；无 path 字段或
/// 相对路径越界（以 .. 开头或为绝对路径）的条目原样保留。
fn remap_plan_paths(entries: Option<&Value>, from_dir: &str, to_dir: &str) -> Vec<Value> {
    let Some(items) = entries.and_then(Value::as_array) else {
        return Vec::new();
    };
    if from_dir.is_empty() || to_dir.is_empty() || from_dir == to_dir {
        return items.clone();
    }
    items
        .iter()
        .map(|entry| {
            let Some(path) = entry.get("path").and_then(Value::as_str) else {
                return entry.clone();
            };
            let trimmed_path = path.trim();
            let relative = path_relative(from_dir, trimmed_path);
            if !relative.is_empty()
                && (relative.starts_with("..") || Path::new(&relative).is_absolute())
            {
                return entry.clone();
            }
            let mut next = entry.clone();
            if let Some(map) = next.as_object_mut() {
                let new_path = if relative.is_empty() {
                    to_dir.to_string()
                } else {
                    format!("{to_dir}{}{relative}", std::path::MAIN_SEPARATOR)
                };
                map.insert("path".to_string(), Value::from(new_path));
            }
            next
        })
        .collect()
}

/// `mergeProjectConfigData`: identity-merge the server-owned project config
/// across a storage directory change.
/// 跨存储目录变更合并服务端拥有的项目配置：新旧对象 spread 后按字段
/// 规则合并——projectPath 修正为当前路径、notes 取非空一侧、
/// setup-worktree 去重拼接、todos/actions/scheduledTasks/planFiles 按键
/// 去重（新条目优先）、primaryId 取新侧回退旧侧。
fn merge_project_config_data(
    old_config: Option<&Value>,
    new_config: Option<&Value>,
    old_storage_dir: &Path,
    new_storage_dir: &Path,
    project_path: Option<&str>,
) -> Value {
    let old_value = value_object(old_config);
    let new_value = value_object(new_config);
    let old_dir = old_storage_dir.to_string_lossy().into_owned();
    let new_dir = new_storage_dir.to_string_lossy().into_owned();

    let old_plan_files = remap_plan_paths(old_value.get("projectPlanFiles"), &old_dir, &new_dir);
    let new_plan_files = remap_plan_paths(new_value.get("projectPlanFiles"), &old_dir, &new_dir);
    let old_notes = old_value
        .get("projectNotes")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let new_notes = new_value
        .get("projectNotes")
        .and_then(Value::as_str)
        .unwrap_or_default();

    let mut merged: Map<String, Value> = Map::new();
    spread_into(&mut merged, &Value::Object(old_value.clone()));
    spread_into(&mut merged, &Value::Object(new_value.clone()));
    if let Some(project_path) = project_path.map(str::trim).filter(|s| !s.is_empty()) {
        merged.insert("projectPath".into(), Value::from(project_path));
    }
    let setup_worktree = unique_strings(
        old_value
            .get("setup-worktree")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .chain(
                new_value
                    .get("setup-worktree")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten(),
            )
            .cloned(),
    );
    if !setup_worktree.is_empty() {
        merged.insert(
            "setup-worktree".into(),
            Value::Array(setup_worktree.into_iter().map(Value::from).collect()),
        );
    }
    if !old_notes.is_empty() || !new_notes.is_empty() {
        let notes = if new_notes.is_empty() {
            old_notes
        } else {
            new_notes
        };
        merged.insert("projectNotes".into(), Value::from(notes));
    }
    let todos = merge_by_key(
        old_value.get("projectTodos").and_then(Value::as_array),
        new_value.get("projectTodos").and_then(Value::as_array),
        &["id"],
    );
    if !todos.is_empty() {
        merged.insert("projectTodos".into(), Value::Array(todos));
    }
    let actions = merge_by_key(
        old_value.get("projectActions").and_then(Value::as_array),
        new_value.get("projectActions").and_then(Value::as_array),
        &["id"],
    );
    if !actions.is_empty() {
        merged.insert("projectActions".into(), Value::Array(actions));
    }
    let scheduled = merge_by_key(
        old_value.get("scheduledTasks").and_then(Value::as_array),
        new_value.get("scheduledTasks").and_then(Value::as_array),
        &["id"],
    );
    if !scheduled.is_empty() {
        merged.insert("scheduledTasks".into(), Value::Array(scheduled));
    }
    let plan_files = merge_by_key(
        Some(&old_plan_files),
        Some(&new_plan_files),
        &["id", "path"],
    );
    if !plan_files.is_empty() {
        merged.insert("projectPlanFiles".into(), Value::Array(plan_files));
    }
    let primary = new_value
        .get("projectActionsPrimaryId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .or_else(|| {
            old_value
                .get("projectActionsPrimaryId")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
        });
    if let Some(primary) = primary {
        merged.insert("projectActionsPrimaryId".into(), Value::from(primary));
    }
    Value::Object(merged)
}

/// `moveDirectoryContents`: rename everything that does not already exist at
/// the destination, then drop the source directory.
/// 把 from_dir 的内容并入 to_dir：子目录递归、目标已存在的文件跳过
/// （对齐 JS access 判定）、其余 rename，最后删除源目录；源目录不存在
/// 直接视为成功。
async fn move_directory_contents(from_dir: &Path, to_dir: &Path) -> Result<(), AppError> {
    let entries = match tokio::fs::read_dir(from_dir).await {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    tokio::fs::create_dir_all(to_dir).await?;

    let mut entries = entries;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name();
        let from_path = from_dir.join(&name);
        let to_path = to_dir.join(&name);
        if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
            Box::pin(move_directory_contents(&from_path, &to_path)).await?;
            continue;
        }
        // JS `access()` treats every error as "destination exists" → skip.
        if tokio::fs::metadata(&to_path).await.is_ok() {
            continue;
        }
        tokio::fs::rename(&from_path, &to_path).await?;
    }

    remove_dir_force(from_dir).await
}

/// `migrateProjectIconFiles`: move `project-<sha1(id)>.<ext>` icons.
/// 迁移项目图标：把 project-<sha1(id)>.<ext> 逐扩展名改名到新 id；
/// 新图标已存在时直接删除旧图标。
async fn migrate_project_icon_files(
    icons_dir: &Path,
    old_id: &str,
    new_id: &str,
) -> Result<(), AppError> {
    if old_id.is_empty() || new_id.is_empty() || old_id == new_id {
        return Ok(());
    }
    let old_base = format!("project-{}", sha1_hex(old_id));
    let new_base = format!("project-{}", sha1_hex(new_id));
    tokio::fs::create_dir_all(icons_dir).await?;

    for ext in PROJECT_ICON_EXTENSIONS {
        let old_path = icons_dir.join(format!("{old_base}.{ext}"));
        let new_path = icons_dir.join(format!("{new_base}.{ext}"));
        match tokio::fs::metadata(&old_path).await {
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err.into()),
        }
        if tokio::fs::metadata(&new_path).await.is_err() {
            tokio::fs::rename(&old_path, &new_path).await?;
            continue;
        }
        remove_file_force(&old_path).await?;
    }
    Ok(())
}

/// `mergeProjectContextFiles`: merge server-owned context.json (notes/todos/
/// plans) across a project id change so neither side loses entries.
/// 跨项目 id 变更合并 context.json（notes/todos/plans）：两侧都有文件时
/// 按身份键去重合并（v1 字符串形态的 notes 优先保留数组、双侧都是
/// 字符串时取新侧），结果写回新路径并删除旧文件；任一侧缺失时交给
/// 纯目录搬迁处理。
async fn merge_project_context_files(
    old_storage_dir: &Path,
    new_storage_dir: &Path,
) -> Result<(), AppError> {
    let old_context_path = old_storage_dir.join("context.json");
    let new_context_path = new_storage_dir.join("context.json");

    // Both reads coerce every failure to null (JS `.catch(() => null)`).
    let old_context = read_json_file(&old_context_path).await.unwrap_or(None);
    let new_context = read_json_file(&new_context_path).await.unwrap_or(None);
    let (Some(old_context), Some(new_context)) = (old_context, new_context) else {
        // Nothing to reconcile: the plain directory move handles one side.
        return Ok(());
    };

    // One side may still be a version 1 string; keep whichever is a list, and
    // prefer the destination when both are strings.
    let merged_notes: Value = match (old_context.get("notes"), new_context.get("notes")) {
        (Some(old_notes @ Value::Array(_)), Some(new_notes @ Value::Array(_))) => Value::Array(
            merge_by_key(old_notes.as_array(), new_notes.as_array(), &["id"]),
        ),
        (_, Some(Value::Array(notes))) => Value::Array(notes.clone()),
        (Some(Value::Array(notes)), _) => Value::Array(notes.clone()),
        _ => new_context
            .get("notes")
            .cloned()
            .or_else(|| old_context.get("notes").cloned())
            .unwrap_or(Value::from("")),
    };

    let mut merged: Map<String, Value> = Map::new();
    spread_into(&mut merged, &old_context);
    spread_into(&mut merged, &new_context);
    merged.insert("notes".into(), merged_notes);
    merged.insert(
        "todos".into(),
        Value::Array(merge_by_key(
            old_context.get("todos").and_then(Value::as_array),
            new_context.get("todos").and_then(Value::as_array),
            &["id"],
        )),
    );
    merged.insert(
        "plans".into(),
        Value::Array(merge_by_key(
            old_context.get("plans").and_then(Value::as_array),
            new_context.get("plans").and_then(Value::as_array),
            &["id", "file"],
        )),
    );
    write_json_file(&new_context_path, &Value::Object(merged)).await?;
    remove_file_force(&old_context_path).await
}

/// `migrateProjectScopedStorage`: move `<projectsRoot>/<id>.json` + the
/// per-project storage directory across a project id change.
/// 迁移项目作用域存储：合并新旧 <id>.json 配置、合并 context.json、
/// 搬迁剩余目录内容并删除旧配置文件；新旧 id 相同或为空则直接跳过。
async fn migrate_project_scoped_storage(
    projects_root: &Path,
    old_id: &str,
    new_id: &str,
    project_path: &str,
) -> Result<(), AppError> {
    if old_id.is_empty() || new_id.is_empty() || old_id == new_id {
        return Ok(());
    }
    let old_config_path = projects_root.join(format!("{old_id}.json"));
    let new_config_path = projects_root.join(format!("{new_id}.json"));
    let old_storage_dir = projects_root.join(old_id);
    let new_storage_dir = projects_root.join(new_id);

    let old_config = read_json_file(&old_config_path).await?;
    let new_config = read_json_file(&new_config_path).await?;

    if old_config.is_some() || new_config.is_some() {
        let merged = merge_project_config_data(
            old_config.as_ref(),
            new_config.as_ref(),
            &old_storage_dir,
            &new_storage_dir,
            Some(project_path),
        );
        write_json_file(&new_config_path, &merged).await?;
    }

    merge_project_context_files(&old_storage_dir, &new_storage_dir).await?;
    move_directory_contents(&old_storage_dir, &new_storage_dir).await?;
    remove_file_force(&old_config_path).await
}

// ---------------------------------------------------------------------------
// Orphan recovery
// ---------------------------------------------------------------------------

/// 取路径最后一段并小写（统一把 `\` 视为分隔符），供孤儿指纹匹配使用。
fn basename_of(project_path: Option<&str>) -> String {
    let Some(path) = project_path else {
        return String::new();
    };
    let normalized = path.replace('\\', "/");
    let trimmed = normalized.trim_end_matches('/');
    let idx = trimmed.rfind('/').map(|i| i + 1).unwrap_or(0);
    trimmed[idx..].to_lowercase()
}

/// Scan `$ROOT_PROJECT_PATH/…` / `$ROOT_WORKTREE_PATH/…` references in
/// setup-worktree commands and project action commands (JS regex
/// `/\$(?:\{)?ROOT_(?:PROJECT|WORKTREE)_PATH\}?\/([A-Za-z0-9._/-]+)/g`).
/// 从 setup-worktree 与 projectActions 命令里扫描
/// $ROOT_PROJECT_PATH/…、$ROOT_WORKTREE_PATH/… 引用，返回去重后的
/// 相对路径片段（手写扫描等价 JS 正则，避免正则引擎差异）。
fn extract_root_rel_paths(orphan: &Value) -> Vec<String> {
    let mut commands: Vec<&str> = Vec::new();
    if let Some(setup) = orphan.get("setup-worktree").and_then(Value::as_array) {
        commands.extend(setup.iter().filter_map(Value::as_str));
    }
    if let Some(actions) = orphan.get("projectActions").and_then(Value::as_array) {
        commands.extend(
            actions
                .iter()
                .filter_map(|a| a.get("command").and_then(Value::as_str)),
        );
    }

    let is_rel_char = |c: u8| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'/' | b'-');
    let mut results: Vec<String> = Vec::new();
    for command in commands {
        let bytes = command.as_bytes();
        let mut i = 0usize;
        while i < bytes.len() {
            if bytes[i] != b'$' {
                i += 1;
                continue;
            }
            let braced = i + 1 < bytes.len() && bytes[i + 1] == b'{';
            let marker_start = if braced { i + 2 } else { i + 1 };
            let rest = &command[marker_start..];
            let marker_len = if rest.starts_with("ROOT_PROJECT_PATH") {
                "ROOT_PROJECT_PATH".len()
            } else if rest.starts_with("ROOT_WORKTREE_PATH") {
                "ROOT_WORKTREE_PATH".len()
            } else {
                i += 1;
                continue;
            };
            let after_marker = marker_start + marker_len;
            let after_brace = if after_marker < bytes.len() && bytes[after_marker] == b'}' {
                after_marker + 1
            } else {
                after_marker
            };
            if after_brace >= bytes.len() || bytes[after_brace] != b'/' {
                i += 1;
                continue;
            }
            let mut end = after_brace + 1;
            while end < bytes.len() && is_rel_char(bytes[end]) {
                end += 1;
            }
            let captured = command[after_brace + 1..end].to_string();
            if end > after_brace + 1 && !results.contains(&captured) {
                results.push(captured);
            }
            i = end.max(i + 1);
        }
    }
    results
}

/// 路径可访问（metadata 成功）即视为存在。
async fn file_exists(path: &Path) -> bool {
    tokio::fs::metadata(path).await.is_ok()
}

/// 判定孤儿配置是否指向给定项目：先看 ROOT_PROJECT_PATH/ROOT_WORKTREE_PATH
/// 引用的相对路径是否存在于项目目录下，再看命令与动作文本是否包含
/// 项目目录名（小写包含匹配）。
async fn orphan_matches_project(orphan: &Value, project: &Value) -> bool {
    let Some(project_path) = project
        .get("path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return false;
    };
    for rel in extract_root_rel_paths(orphan) {
        if file_exists(&Path::new(project_path).join(&rel)).await {
            return true;
        }
    }
    let name = basename_of(Some(project_path));
    if name.is_empty() {
        return false;
    }
    let mut haystacks = String::new();
    if let Some(setup) = orphan.get("setup-worktree").and_then(Value::as_array) {
        for entry in setup {
            if let Some(s) = entry.as_str() {
                haystacks.push(' ');
                haystacks.push_str(s);
            }
        }
    }
    if let Some(actions) = orphan.get("projectActions").and_then(Value::as_array) {
        for action in actions {
            let name_part = action
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let command_part = action
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or_default();
            haystacks.push(' ');
            haystacks.push_str(name_part);
            haystacks.push(' ');
            haystacks.push_str(command_part);
        }
    }
    haystacks.to_lowercase().contains(&name)
}

/// `recoverOrphanProjectFiles`: match unreferenced random-UUID project
/// configs to canonical projects by their setup-worktree fingerprints.
/// 孤儿恢复：扫描 projects 根下未被引用的随机 UUID 配置（跳过 path_
/// 前缀与已知 id），只处理含 notes/todos/actions/setup/plan 内容的文件；
/// 指纹唯一命中某个规范项目时合并配置并搬迁存储目录，其余记告警。
async fn recover_orphan_project_files(
    projects_root: &Path,
    canonical_projects: &[Value],
) -> Result<(), AppError> {
    let entries = match tokio::fs::read_dir(projects_root).await {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };

    let canonical_ids: HashSet<String> = canonical_projects
        .iter()
        .filter_map(|p| p.get("id").and_then(Value::as_str).map(String::from))
        .collect();

    let mut orphan_files: Vec<String> = Vec::new();
    let mut entries = entries;
    while let Some(entry) = entries.next_entry().await? {
        let is_file = entry
            .file_type()
            .await
            .map(|t| t.is_file())
            .unwrap_or(false);
        if !is_file {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(id) = name.strip_suffix(".json") else {
            continue;
        };
        if id.is_empty() || id.starts_with("path_") || canonical_ids.contains(id) {
            continue;
        }
        orphan_files.push(id.to_string());
    }

    if orphan_files.is_empty() {
        return Ok(());
    }
    tracing::warn!(
        "[projects] Found {} orphan project config(s) without projectPath.",
        orphan_files.len()
    );

    let mut orphans: Vec<(String, PathBuf, Value)> = Vec::new();
    for orphan_id in &orphan_files {
        let file_path = projects_root.join(format!("{orphan_id}.json"));
        let Some(content) = read_json_file(&file_path).await? else {
            continue;
        };
        let has_content = [
            content
                .get("projectNotes")
                .and_then(Value::as_str)
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false),
            content
                .get("projectTodos")
                .and_then(Value::as_array)
                .is_some_and(|a| !a.is_empty()),
            content
                .get("projectActions")
                .and_then(Value::as_array)
                .is_some_and(|a| !a.is_empty()),
            content
                .get("setup-worktree")
                .and_then(Value::as_array)
                .is_some_and(|a| !a.is_empty()),
            content
                .get("projectPlanFiles")
                .and_then(Value::as_array)
                .is_some_and(|a| !a.is_empty()),
        ]
        .into_iter()
        .any(|flag| flag);
        if !has_content {
            continue;
        }
        orphans.push((orphan_id.clone(), file_path, content));
    }
    if orphans.is_empty() {
        return Ok(());
    }

    let mut matches: HashMap<String, Vec<usize>> = HashMap::new();
    for (index, (_, _, content)) in orphans.iter().enumerate() {
        let mut matched_projects: Vec<usize> = Vec::new();
        for (project_index, project) in canonical_projects.iter().enumerate() {
            if orphan_matches_project(content, project).await {
                matched_projects.push(project_index);
            }
        }
        if matched_projects.len() == 1 {
            let project = &canonical_projects[matched_projects[0]];
            let project_id = project
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            matches.entry(project_id).or_default().push(index);
        }
    }

    let mut orphans_consumed: HashSet<String> = HashSet::new();
    for (project_id, orphan_indexes) in &matches {
        let Some(project) = canonical_projects
            .iter()
            .find(|p| p.get("id").and_then(Value::as_str) == Some(project_id.as_str()))
        else {
            continue;
        };
        let target_path = projects_root.join(format!("{project_id}.json"));
        let project_path = project
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        for index in orphan_indexes {
            let (orphan_id, orphan_file, content) = &orphans[*index];
            let target_existing = read_json_file(&target_path)
                .await?
                .unwrap_or(Value::Object(Map::new()));
            let merged = merge_project_config_data(
                Some(content),
                Some(&target_existing),
                &projects_root.join(orphan_id),
                &projects_root.join(project_id),
                Some(&project_path),
            );
            write_json_file(&target_path, &merged).await?;
            move_directory_contents(
                &projects_root.join(orphan_id),
                &projects_root.join(project_id),
            )
            .await?;
            remove_file_force(orphan_file).await?;
            orphans_consumed.insert(orphan_id.clone());
            tracing::info!(
                "[projects] Recovered orphan {orphan_id} -> {project_id} ({project_path})"
            );
        }
    }

    let remaining: Vec<&String> = orphans
        .iter()
        .map(|(id, _, _)| id)
        .filter(|id| !orphans_consumed.contains(*id))
        .collect();
    if !remaining.is_empty() {
        let ids: Vec<&str> = remaining.iter().map(|s| s.as_str()).collect();
        tracing::warn!(
            "[projects] {} orphan project file(s) could not be auto-matched: {}",
            remaining.len(),
            ids.join(", ")
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Settings migrations
// ---------------------------------------------------------------------------

/// 各迁移函数的统一返回：迁移后的 settings 与是否发生改动。
type MigrationResult = Result<(Map<String, Value>, bool), AppError>;

/// 取 settings 中某键的字符串值；非字符串得 None。
fn settings_str<'a>(settings: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    settings.get(key).and_then(Value::as_str)
}

/// `resolveDirectoryCandidate` (project-directory-runtime.js): trim,
/// normalize, resolve — no filesystem access.
/// 目录候选解析：trim → 规范化 → resolve，全程不做文件系统访问。
fn resolve_directory_candidate(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let normalized = normalize_directory_path(trimmed);
    Some(path_resolve(&normalized).to_string_lossy().into_owned())
}

/// 路径存在且为目录。
async fn is_directory(path: &str) -> bool {
    matches!(tokio::fs::metadata(path).await, Ok(md) if md.is_dir())
}

/// Migration 1: seed projects from the legacy `lastDirectory` key and keep
/// `activeProjectId` pointing at a live project.
/// 项目列表为空且 lastDirectory 仍指向现存目录时，以其播种首个项目并
/// 设为激活；同时保证 activeProjectId 始终指向存活项目（否则改指首个，
/// 无项目时清除）。
async fn migrate_settings_from_legacy_last_directory(
    settings: &Map<String, Value>,
) -> MigrationResult {
    let mut next_projects = sanitize_projects(settings.get("projects")).unwrap_or_default();
    let mut next_active_project_id = settings_str(settings, "activeProjectId").map(String::from);
    let mut changed = false;

    if next_projects.is_empty() {
        let legacy = settings_str(settings, "lastDirectory")
            .map(str::trim)
            .unwrap_or_default();
        if !legacy.is_empty()
            && let Some(candidate) = resolve_directory_candidate(legacy)
            && is_directory(&candidate).await
        {
            let id = create_project_id_from_path(&candidate);
            next_projects = vec![serde_json::json!({
                "id": id,
                "path": candidate,
                "addedAt": now_ms(),
                "lastOpenedAt": now_ms(),
            })];
            next_active_project_id = Some(id);
            changed = true;
        }
    }

    if !next_projects.is_empty() {
        let active_exists = next_active_project_id
            .as_deref()
            .map(|active| {
                next_projects
                    .iter()
                    .any(|p| p.get("id").and_then(Value::as_str) == Some(active))
            })
            .unwrap_or(false);
        if !active_exists {
            next_active_project_id = next_projects[0]
                .get("id")
                .and_then(Value::as_str)
                .map(String::from);
            changed = true;
        }
    } else if let Some(active) = &next_active_project_id
        && !active.is_empty()
    {
        next_active_project_id = None;
        changed = true;
    }

    if !changed {
        return Ok((settings.clone(), false));
    }

    let mut changes: SanitizedUpdate = SanitizedUpdate::new();
    changes.insert("projects".into(), Some(Value::Array(next_projects)));
    match next_active_project_id {
        Some(active) => {
            changes.insert("activeProjectId".into(), Some(Value::from(active)));
        }
        None => {
            changes.insert("activeProjectId".into(), None);
        }
    }
    Ok((merge_persisted_settings(settings, &changes), true))
}

/// Migration 2: split the legacy single-theme preference into light/dark ids.
/// 把单一 themeId/themeVariant 偏好拆成 lightThemeId 与 darkThemeId：
/// 变体匹配时沿用旧主题 id，否则填默认主题；两侧齐备则不改动。
fn migrate_settings_from_legacy_theme_preferences(
    settings: &Map<String, Value>,
) -> MigrationResult {
    let theme_id = settings_str(settings, "themeId")
        .map(str::trim)
        .unwrap_or_default();
    let theme_variant = settings_str(settings, "themeVariant")
        .map(str::trim)
        .unwrap_or_default();

    let has_light = settings_str(settings, "lightThemeId")
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    let has_dark = settings_str(settings, "darkThemeId")
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    if has_light && has_dark {
        return Ok((settings.clone(), false));
    }

    let mut changes: SanitizedUpdate = SanitizedUpdate::new();
    if !has_light {
        let next = if !theme_id.is_empty() && theme_variant == "light" {
            settings_str(settings, "themeId")
                .unwrap_or_default()
                .to_string()
        } else {
            DEFAULT_LIGHT_THEME_ID.to_string()
        };
        if !next.is_empty() {
            changes.insert("lightThemeId".into(), Some(Value::from(next)));
        }
    }
    if !has_dark {
        let next = if !theme_id.is_empty() && theme_variant == "dark" {
            settings_str(settings, "themeId")
                .unwrap_or_default()
                .to_string()
        } else {
            DEFAULT_DARK_THEME_ID.to_string()
        };
        if !next.is_empty() {
            changes.insert("darkThemeId".into(), Some(Value::from(next)));
        }
    }
    Ok((merge_persisted_settings(settings, &changes), true))
}

/// Migration 3: fold `collapsedProjects` into per-project `sidebarCollapsed`.
/// 把旧版全局 collapsedProjects id 列表折叠进各项目的 sidebarCollapsed
/// 布尔位，并移除遗留键。
fn migrate_settings_from_legacy_collapsed_projects(
    settings: &Map<String, Value>,
) -> MigrationResult {
    let collapsed = settings
        .get("collapsedProjects")
        .and_then(Value::as_array)
        .map(|items| normalize_string_array(&Value::Array(items.clone())))
        .unwrap_or_default();

    if collapsed.is_empty() || settings.get("projects").and_then(Value::as_array).is_none() {
        if collapsed.is_empty() {
            return Ok((settings.clone(), false));
        }
        let mut next = settings.clone();
        next.remove("collapsedProjects");
        return Ok((next, true));
    }

    let collapsed_set: HashSet<&str> = collapsed.iter().map(String::as_str).collect();
    let projects = sanitize_projects(settings.get("projects")).unwrap_or_default();
    let mut changed = false;

    let next_projects: Vec<Value> = projects
        .iter()
        .map(|project| {
            let should_collapse = project
                .get("id")
                .and_then(Value::as_str)
                .map(|id| collapsed_set.contains(id))
                .unwrap_or(false);
            let current = project.get("sidebarCollapsed").and_then(Value::as_bool);
            if current != Some(should_collapse) {
                changed = true;
                let mut next = project.clone();
                if let Some(map) = next.as_object_mut() {
                    map.insert("sidebarCollapsed".into(), Value::from(should_collapse));
                }
                next
            } else {
                project.clone()
            }
        })
        .collect();

    if !changed {
        if settings.contains_key("collapsedProjects") {
            let mut next = settings.clone();
            next.remove("collapsedProjects");
            return Ok((next, true));
        }
        return Ok((settings.clone(), false));
    }

    let mut next = settings.clone();
    next.insert("projects".into(), Value::Array(next_projects));
    next.remove("collapsedProjects");
    Ok((next, true))
}

/// Migration 4: default notification booleans + template shape.
/// 补齐四类通知开关的布尔默认值（缺省 true），并修复
/// notificationTemplates 的形状（见 ensure_notification_template_shape）。
fn migrate_settings_notification_defaults(settings: &Map<String, Value>) -> MigrationResult {
    let mut next = settings.clone();
    let mut changed = false;

    for key in [
        "notifyOnSubtasks",
        "notifyOnCompletion",
        "notifyOnError",
        "notifyOnQuestion",
    ] {
        if settings.get(key).and_then(Value::as_bool).is_none() {
            next.insert(key.to_string(), Value::from(true));
            changed = true;
        }
    }

    let (templates, templates_changed) =
        ensure_notification_template_shape(settings.get("notificationTemplates"));
    let templates_present = settings
        .get("notificationTemplates")
        .map(|v| v.as_object().is_some())
        .unwrap_or(false);
    if templates_changed || !templates_present {
        next.insert("notificationTemplates".into(), Value::Object(templates));
        changed = true;
    }

    Ok((if changed { next } else { settings.clone() }, changed))
}

/// Migration 5: rename `namedTunnel*` keys to `managedRemoteTunnel*`.
/// 五个 namedTunnel* 遗留键改名为 managedRemoteTunnel*：改名前先经
/// normalization 清洗，非法值不落新键，最后删除全部遗留键。
fn migrate_settings_from_legacy_named_tunnel_keys(
    settings: &Map<String, Value>,
) -> MigrationResult {
    let mut next = settings.clone();
    let mut changed = false;

    if !next.contains_key("managedRemoteTunnelHostname") && next.contains_key("namedTunnelHostname")
    {
        match normalize_managed_remote_tunnel_hostname(
            next.get("namedTunnelHostname").unwrap_or(&Value::Null),
        ) {
            Some(hostname) => {
                next.insert("managedRemoteTunnelHostname".into(), Value::from(hostname));
            }
            None => {
                next.remove("managedRemoteTunnelHostname");
            }
        }
        changed = true;
    }

    if !next.contains_key("managedRemoteTunnelToken") && next.contains_key("namedTunnelToken") {
        match next.get("namedTunnelToken") {
            Some(Value::Null) => {
                next.insert("managedRemoteTunnelToken".into(), Value::Null);
            }
            Some(Value::String(token)) => {
                next.insert("managedRemoteTunnelToken".into(), Value::from(token.trim()));
            }
            _ => {}
        }
        changed = true;
    }

    if !next.contains_key("managedRemoteTunnelPresets") && next.contains_key("namedTunnelPresets") {
        match normalize_managed_remote_tunnel_presets(
            next.get("namedTunnelPresets").unwrap_or(&Value::Null),
        ) {
            Some(presets) => {
                next.insert("managedRemoteTunnelPresets".into(), Value::Array(presets));
            }
            None => {
                next.remove("managedRemoteTunnelPresets");
            }
        }
        changed = true;
    }

    if !next.contains_key("managedRemoteTunnelPresetTokens")
        && next.contains_key("namedTunnelPresetTokens")
    {
        match normalize_managed_remote_tunnel_preset_tokens(
            next.get("namedTunnelPresetTokens").unwrap_or(&Value::Null),
        ) {
            Some(tokens) => {
                next.insert(
                    "managedRemoteTunnelPresetTokens".into(),
                    Value::Object(tokens),
                );
            }
            None => {
                next.remove("managedRemoteTunnelPresetTokens");
            }
        }
        changed = true;
    }

    if !next.contains_key("managedRemoteTunnelSelectedPresetId")
        && next.contains_key("namedTunnelSelectedPresetId")
    {
        if let Some(selected) = next
            .get("namedTunnelSelectedPresetId")
            .and_then(Value::as_str)
        {
            let selected = selected.trim();
            if !selected.is_empty() {
                next.insert(
                    "managedRemoteTunnelSelectedPresetId".into(),
                    Value::from(selected),
                );
            }
        }
        changed = true;
    }

    for legacy_key in [
        "namedTunnelHostname",
        "namedTunnelToken",
        "namedTunnelPresets",
        "namedTunnelPresetTokens",
        "namedTunnelSelectedPresetId",
    ] {
        if next.remove(legacy_key).is_some() {
            changed = true;
        }
    }

    Ok((if changed { next } else { settings.clone() }, changed))
}

/// Migration 7: move project ids (and their scoped storage) to the
/// deterministic `path_<base64url>` ids.
/// 项目 id（及其作用域存储、图标文件）迁移到路径派生的确定性
/// path_<base64url> id；迁移途中顺带做一次性孤儿恢复，最后按映射表
/// 重定向 activeProjectId。
async fn migrate_settings_to_deterministic_project_ids(
    projects_root: &Path,
    icons_dir: &Path,
    settings: &Map<String, Value>,
    orphan_recovery_done: &AtomicBool,
) -> MigrationResult {
    let projects = sanitize_projects(settings.get("projects")).unwrap_or_default();
    if projects.is_empty() {
        return Ok((settings.clone(), false));
    }

    let mut changed = false;
    let mut project_id_map: HashMap<String, String> = HashMap::new();
    let mut next_projects: Vec<Value> = Vec::new();

    for project in &projects {
        let project_path = project
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let old_id = project
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let canonical_id = create_project_id_from_path(&project_path);
        let next_id = if canonical_id.is_empty() {
            old_id.clone()
        } else {
            canonical_id
        };
        project_id_map.insert(old_id.clone(), next_id.clone());
        if next_id != old_id {
            changed = true;
            migrate_project_scoped_storage(projects_root, &old_id, &next_id, &project_path).await?;
            migrate_project_icon_files(icons_dir, &old_id, &next_id).await?;
        }
        let mut next = project.clone();
        if let Some(map) = next.as_object_mut() {
            map.insert("id".to_string(), Value::from(next_id));
        }
        next_projects.push(next);
    }

    // Orphan recovery is a one-shot best-effort scan per process.
    if !orphan_recovery_done.swap(true, Ordering::SeqCst)
        && let Err(err) = recover_orphan_project_files(projects_root, &next_projects).await
    {
        tracing::warn!("[projects] Orphan recovery failed, continuing startup: {err}");
    }

    if !changed {
        return Ok((settings.clone(), false));
    }

    let current_active_id = settings_str(settings, "activeProjectId")
        .unwrap_or_default()
        .to_string();
    let next_active_project_id = project_id_map
        .get(&current_active_id)
        .cloned()
        .filter(|id| !id.is_empty())
        .or_else(|| (!current_active_id.is_empty()).then_some(current_active_id.clone()))
        .or_else(|| {
            next_projects
                .first()
                .and_then(|p| p.get("id").and_then(Value::as_str).map(String::from))
        });

    let mut next = settings.clone();
    next.insert("projects".into(), Value::Array(next_projects));
    if let Some(active) = next_active_project_id {
        next.insert("activeProjectId".into(), Value::from(active));
    }
    Ok((next, true))
}

/// Migration 8: `approvedDirectories` was a write-only registry — strip it.
/// 删除只写不读的 approvedDirectories 注册表键。
fn migrate_settings_remove_approved_directories(settings: &Map<String, Value>) -> MigrationResult {
    if !settings.contains_key("approvedDirectories") {
        return Ok((settings.clone(), false));
    }
    let mut next = settings.clone();
    next.remove("approvedDirectories");
    Ok((next, true))
}

/// Migration 9: drop the retired OpenCode update notification state.
/// 删除已退役的 OpenCode 更新通知状态键。
fn migrate_settings_remove_opencode_update_state(settings: &Map<String, Value>) -> MigrationResult {
    let legacy_keys = [
        "showOpenCodeUpdateNotifications",
        "openCodeUpdateToastDismissedVersion",
    ];
    if !legacy_keys.iter().any(|key| settings.contains_key(*key)) {
        return Ok((settings.clone(), false));
    }
    let mut next = settings.clone();
    for key in legacy_keys {
        next.remove(key);
    }
    Ok((next, true))
}

/// `validateProjectEntries`: drop projects whose path is missing or no longer
/// a directory (keep them on transient/permission errors).
/// 校验项目条目：path 缺失、为空或不再是目录的条目剔除并告警；权限等
/// 瞬态错误保留条目，避免误删用户项目。
pub(crate) async fn validate_project_entries(projects: &[Value]) -> Vec<Value> {
    let mut validated = Vec::new();
    for project in projects {
        let Some(path) = project
            .get("path")
            .and_then(Value::as_str)
            .filter(|p| !p.is_empty())
        else {
            tracing::warn!(
                "[validateProjectEntries] Dropping project entry with missing or empty path"
            );
            continue;
        };
        match tokio::fs::metadata(path).await {
            Ok(md) if md.is_dir() => validated.push(project.clone()),
            Ok(_) => {
                tracing::warn!(
                    "[validateProjectEntries] Dropping project — path is not a directory: {path}"
                );
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                tracing::warn!(
                    "[validateProjectEntries] Dropping project — directory no longer exists: {path}"
                );
            }
            // Permission or transient fs error — keep the project rather than
            // silently losing it from the user's list.
            Err(_) => validated.push(project.clone()),
        }
    }
    validated
}

// ---------------------------------------------------------------------------
// SettingsStore
// ---------------------------------------------------------------------------

/// Owns settings.json persistence for one data directory. Retrieve shared
/// instances through [`super::store_for_path`] so the persist lock and the
/// one-shot migration flags are process-wide (JS module-level state).
/// 负责单个 data 目录下 settings.json 的持久化；经 super::store_for_path
/// 获取共享实例，使持久化锁与一次性迁移标志在进程内唯一（对应 JS 的
/// 模块级状态）。
pub struct SettingsStore {
    /// settings.json 完整路径；projects/ 与 project-icons/ 目录由其父目录派生。
    settings_path: PathBuf,
    /// JS `persistSettingsLock`: serializes read-modify-write cycles.
    /// 串行化读-改-写持久化周期。
    persist_lock: tokio::sync::Mutex<()>,
    /// JS `hasCleanedOrphanedTempFiles`.
    /// 是否已执行过孤儿临时文件清理（一次性）。
    cleaned_orphaned_temp_files: AtomicBool,
    /// JS `orphanRecoveryDone`.
    /// 是否已执行过孤儿项目恢复（一次性）。
    orphan_recovery_done: AtomicBool,
}

/// settings.json 的读取、迁移与原子持久化入口。
impl SettingsStore {
    /// 以 settings.json 路径构建存储；各一次性标志初始为未执行。
    pub fn new(settings_path: PathBuf) -> Self {
        Self {
            settings_path,
            persist_lock: tokio::sync::Mutex::new(()),
            cleaned_orphaned_temp_files: AtomicBool::new(false),
            orphan_recovery_done: AtomicBool::new(false),
        }
    }

    /// settings.json 路径访问器。
    pub fn settings_path(&self) -> &Path {
        &self.settings_path
    }

    /// 项目配置根 `<data>/projects`（settings.json 的兄弟目录）。
    fn projects_root(&self) -> PathBuf {
        self.settings_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("projects")
    }

    /// 项目图标目录 `<data>/project-icons`（settings.json 的兄弟目录）。
    fn project_icons_dir(&self) -> PathBuf {
        self.settings_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("project-icons")
    }

    /// `readSettingsFromDisk` (lenient): missing file, malformed JSON, and
    /// non-object payloads all read as empty settings.
    /// 宽松读取：文件缺失、JSON 损坏或顶层非对象一律返回空 settings（仅告警）。
    pub async fn read_raw(&self) -> Map<String, Value> {
        let raw = match tokio::fs::read_to_string(&self.settings_path).await {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Map::new(),
            Err(err) => {
                tracing::warn!("Failed to read settings file: {err}");
                return Map::new();
            }
        };
        match serde_json::from_str::<Value>(&raw) {
            Ok(Value::Object(map)) => map,
            Ok(_) => Map::new(),
            Err(err) => {
                tracing::warn!("Failed to read settings file: {err}");
                Map::new()
            }
        }
    }

    /// `readSettingsFromDiskStrict`: only a genuinely missing file means "no
    /// settings"; malformed JSON or a non-object payload is an error so
    /// callers that regenerate persisted identity never treat corruption as
    /// a first run.
    /// 严格读取：仅"文件确实不存在"代表无 settings；JSON 损坏或顶层非对象
    /// 返回 Err，避免持久化身份的调用方把损坏误判为首次运行。
    pub async fn read_strict(&self) -> Result<Map<String, Value>, AppError> {
        let raw = match tokio::fs::read_to_string(&self.settings_path).await {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
            Err(err) => return Err(err.into()),
        };
        let parsed: Value = serde_json::from_str(&raw)
            .map_err(|err| AppError::internal(format!("Failed to parse settings file: {err}")))?;
        match parsed {
            Value::Object(map) => Ok(map),
            _ => Err(AppError::internal(
                "Settings file is malformed (non-object payload)",
            )),
        }
    }

    /// Typed view over the lenient raw read for module consumers.
    /// 宽松原始读取的强类型视图，供模块内消费方使用；解析失败按内部错误上报。
    pub async fn read_typed(&self) -> Result<Settings, AppError> {
        let raw = self.read_raw().await;
        serde_json::from_value(Value::Object(raw))
            .map_err(|err| AppError::internal(format!("settings parse failed: {err}")))
    }

    /// `cleanupOrphanedSettingsTempFiles`: best-effort removal of leftover
    /// `settings.json.tmp-*` files from crashed writes.
    /// 尽力清除崩溃写入残留的 settings.json.tmp-* 文件；任何失败静默忽略。
    async fn cleanup_orphaned_settings_temp_files(&self) {
        let Some(dir) = self.settings_path.parent() else {
            return;
        };
        let entries = match tokio::fs::read_dir(dir).await {
            Ok(entries) => entries,
            Err(_) => return,
        };
        let mut entries = entries;
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name();
            if name.to_string_lossy().starts_with("settings.json.tmp-") {
                let _ = tokio::fs::remove_file(dir.join(name)).await;
            }
        }
    }

    /// `writeSettingsToDisk`: atomic replace via temp file + rename with
    /// restrictive permissions (0o700 dir, 0o600 file).
    /// 原子写 settings.json：temp 文件（0o600）+ rename 替换，目录权限 0o700；
    /// 其它进程以普通 readFile+JSON.parse 读取并把解析错误当作空对象，因此
    /// 必须避免写入中途被读到半截 JSON 而在下次读-改-写时清空配置。
    pub async fn write_raw(&self, settings: &Map<String, Value>) -> Result<(), AppError> {
        let settings_directory = self
            .settings_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        tokio::fs::create_dir_all(&settings_directory).await?;
        #[cfg(unix)]
        {
            use crate::os_compat::PermissionsExt;
            let _ = std::fs::set_permissions(
                &settings_directory,
                std::fs::Permissions::from_mode(0o700),
            );
        }

        // Atomic write: other processes read this file with plain readFile +
        // JSON.parse and coerce parse errors to {} — a partial read during a
        // non-atomic write would wipe the settings on their next
        // read-modify-write.
        let tmp = self.settings_path.with_file_name(format!(
            "settings.json.tmp-{}-{}-{:x}",
            std::process::id(),
            now_ms(),
            rand::random::<u32>()
        ));
        let body = serde_json::to_string_pretty(&Value::Object(settings.clone()))
            .map_err(|err| AppError::internal(format!("serialize failed: {err}")))?;
        if let Err(err) = write_file_private(&tmp, body.as_bytes()).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            tracing::warn!("Failed to write settings file: {err}");
            return Err(err);
        }
        if let Err(err) = tokio::fs::rename(&tmp, &self.settings_path).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            tracing::warn!("Failed to write settings file: {err}");
            return Err(err.into());
        }
        #[cfg(unix)]
        {
            use crate::os_compat::PermissionsExt;
            let _ = std::fs::set_permissions(
                &self.settings_path,
                std::fs::Permissions::from_mode(0o600),
            );
        }
        Ok(())
    }

    /// `readSettingsFromDiskMigrated`: lenient read + all startup migrations,
    /// persisting the migrated settings when anything changed.
    /// 宽松读取后依次执行全部启动迁移（含一次性的临时文件清理与孤儿恢复），
    /// 任一迁移产生改动则立即落盘。
    pub async fn read_migrated(&self) -> Result<Map<String, Value>, AppError> {
        if !self
            .cleaned_orphaned_temp_files
            .swap(true, Ordering::SeqCst)
        {
            self.cleanup_orphaned_settings_temp_files().await;
        }
        let current = self.read_raw().await;

        let (settings, c1) = migrate_settings_from_legacy_last_directory(&current).await?;
        let (settings, c2) = migrate_settings_from_legacy_theme_preferences(&settings)?;
        let (settings, c3) = migrate_settings_from_legacy_collapsed_projects(&settings)?;
        let (settings, c4) = migrate_settings_notification_defaults(&settings)?;
        let (settings, c5) = migrate_settings_from_legacy_named_tunnel_keys(&settings)?;
        let (settings, c6) = normalize_settings_paths(&settings);
        let (settings, c7) = migrate_settings_to_deterministic_project_ids(
            &self.projects_root(),
            &self.project_icons_dir(),
            &settings,
            &self.orphan_recovery_done,
        )
        .await?;
        let (settings, c8) = migrate_settings_remove_approved_directories(&settings)?;
        let (settings, c9) = migrate_settings_remove_opencode_update_state(&settings)?;

        if c1 || c2 || c3 || c4 || c5 || c6 || c7 || c8 || c9 {
            self.write_raw(&settings).await?;
        }
        Ok(settings)
    }

    /// `persistSettings`: sanitize an incoming update, merge it into the
    /// persisted settings, run path/id migrations, keep the active project
    /// sane, write atomically, and return the formatted response.
    /// persistSettings：加锁 → 清洗更新 → 合并 → 路径与项目 id 迁移 → 校验
    /// 项目条目 → 修正 activeProjectId → 原子写盘，返回格式化响应；日志只
    /// 记字段名，防止凭据（密码/隧道 token 等）落盘。
    pub async fn persist(&self, changes: &Value) -> Result<Value, AppError> {
        let _guard = self.persist_lock.lock().await;
        // Log field names only — changes can carry credentials (UI password,
        // client tokens, tunnel tokens) that must never reach the log file.
        let field_names = match changes {
            Value::Object(map) if !map.is_empty() => {
                map.keys().cloned().collect::<Vec<_>>().join(", ")
            }
            _ => String::new(),
        };
        tracing::info!(
            "[persistSettings] Updating fields: {}",
            if field_names.is_empty() {
                "(none)"
            } else {
                &field_names
            }
        );

        let current = self.read_raw().await;
        let sanitized = sanitize_settings_update(changes)?;
        let mut next = merge_persisted_settings(&current, &sanitized);

        let (normalized, changed) = normalize_settings_paths(&next);
        if changed {
            next = normalized;
        }

        let (migrated, changed) = migrate_settings_to_deterministic_project_ids(
            &self.projects_root(),
            &self.project_icons_dir(),
            &next,
            &self.orphan_recovery_done,
        )
        .await?;
        if changed {
            next = migrated;
        }

        let (cleaned, changed) = migrate_settings_remove_approved_directories(&next)?;
        if changed {
            next = cleaned;
        }

        // Validating project paths hits the filesystem for every entry, so
        // only do it when the incoming update actually touches the list.
        if sanitized.contains_key("projects")
            && let Some(projects) = next.get("projects").and_then(Value::as_array).cloned()
        {
            let validated = validate_project_entries(&projects).await;
            next.insert("projects".into(), Value::Array(validated));
        }

        let projects = next
            .get("projects")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if !projects.is_empty() {
            let active_id = next
                .get("activeProjectId")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let active = !active_id.is_empty()
                && projects
                    .iter()
                    .any(|p| p.get("id").and_then(Value::as_str) == Some(active_id));
            if !active {
                let first = projects[0]
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                tracing::info!(
                    "[persistSettings] Active project ID {active_id} not found, switching to {first}"
                );
                next.insert("activeProjectId".into(), Value::from(first));
            }
        } else if next.get("activeProjectId").map(js_truthy).unwrap_or(false)
            && let Some(value) = next.remove("activeProjectId")
            && let Some(id) = value.as_str()
        {
            tracing::info!("[persistSettings] No projects found, clearing activeProjectId {id}");
        }

        // NOTE (port gap): the JS runtime also syncs the managed remote
        // tunnel config (`syncManagedRemoteTunnelConfigWithPresets` /
        // `upsertManagedRemoteTunnelToken`) when an update touches
        // `managedRemoteTunnelPresets` / `managedRemoteTunnelPresetTokens`.
        // That wiring belongs to the tunnels module port; the settings-side
        // side effects are not performed here yet.

        self.write_raw(&next).await?;
        format_settings_response(&next)
    }
}

/// Write a file with 0o600 permissions (mode applies on create; an explicit
/// chmod defeats umask, mirroring the JS sequence).
/// 以 0o600 权限写文件：创建时指定 mode 并显式 chmod 压过 umask；
/// tokio File 需 flush 完成后才返回，保证同进程的同步读者不会观察到
/// 截断/空内容。
async fn write_file_private(path: &Path, bytes: &[u8]) -> Result<(), AppError> {
    #[cfg(unix)]
    {
        use crate::os_compat::PermissionsExt;
        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .await?;
        // tokio::fs::File buffers internally and flushes from a background
        // task on drop — same-process std::fs readers (fs workspace
        // resolution reads settings.json synchronously) must not observe a
        // truncated/empty file after persist returns.
        file.write_all(bytes).await?;
        file.flush().await?;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    {
        tokio::fs::write(path, bytes).await?;
    }
    Ok(())
}
