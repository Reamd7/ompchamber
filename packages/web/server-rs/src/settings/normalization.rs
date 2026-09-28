//! Port of `server/lib/opencode/settings-normalization-runtime.js` plus the
//! tunnel type normalizers it needs from `server/lib/tunnels/types.js`
//! (`normalizeTunnelProvider`, `normalizeTunnelMode`, `normalizeOptionalPath`)
//! and `server/lib/projects/project-id.js` (`createProjectIdFromPath`).
//!
//! All helpers mirror the JS dynamic-typing semantics: they take
//! `serde_json::Value` inputs and apply the same `typeof`/`Array.isArray`
//! guards, so malformed persisted shapes degrade exactly like the JS.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::error::AppError;

// index.js production wiring for createSettingsNormalizationRuntime.
const TUNNEL_BOOTSTRAP_TTL_DEFAULT_MS: f64 = 30.0 * 60.0 * 1000.0;
const TUNNEL_BOOTSTRAP_TTL_MIN_MS: f64 = 60.0 * 1000.0;
const TUNNEL_BOOTSTRAP_TTL_MAX_MS: f64 = 24.0 * 60.0 * 60.0 * 1000.0;
const TUNNEL_SESSION_TTL_DEFAULT_MS: f64 = 8.0 * 60.0 * 60.0 * 1000.0;
const TUNNEL_SESSION_TTL_MIN_MS: f64 = 5.0 * 60.0 * 1000.0;
const TUNNEL_SESSION_TTL_MAX_MS: f64 = 30.0 * 24.0 * 60.0 * 60.0 * 1000.0;

const TUNNEL_PROVIDER_CLOUDFLARE: &str = "cloudflare";

// ---------------------------------------------------------------------------
// JS value helpers
// ---------------------------------------------------------------------------

/// JS truthiness for a JSON value (undefined/null/false/0/"" are falsy).
pub(crate) fn js_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// `typeof x === 'number' && Number.isFinite(x)`.
pub(crate) fn finite_number(v: &Value) -> Option<f64> {
    v.as_f64().filter(|f| f.is_finite())
}

/// `Number.isSafeInteger(x)` for a JSON value.
pub(crate) fn is_safe_integer(v: &Value) -> Option<i64> {
    let f = finite_number(v)?;
    if f.fract() != 0.0 || f.abs() > 9_007_199_254_740_991.0 {
        return None;
    }
    Some(f as i64)
}

/// JS `Math.round` (halfway cases round toward +Infinity).
pub(crate) fn js_round(x: f64) -> f64 {
    (x + 0.5).floor()
}

/// Serialize a finite number the way JSON.stringify would (integral values
/// print without a decimal point).
pub(crate) fn num_to_json(x: f64) -> Value {
    if x.fract() == 0.0 && x.abs() <= 9_007_199_254_740_991.0 {
        Value::from(x as i64)
    } else {
        Value::from(x)
    }
}

pub(crate) fn clamp_number(value: f64, min: f64, max: f64) -> f64 {
    value.max(min).min(max)
}

/// JS `Object.entries`: object keys (or array indices) paired with values.
pub(crate) fn object_entries(v: &Value) -> Option<Vec<(String, Value)>> {
    match v {
        Value::Object(map) => Some(
            map.iter()
                .map(|(k, val)| (k.clone(), val.clone()))
                .collect(),
        ),
        Value::Array(items) => Some(
            items
                .iter()
                .enumerate()
                .map(|(i, val)| (i.to_string(), val.clone()))
                .collect(),
        ),
        _ => None,
    }
}

/// JS object spread `{ ...target, ...source }`: object entries, array index
/// keys, or UTF-16 string indices.
pub(crate) fn spread_into(target: &mut Map<String, Value>, source: &Value) {
    match source {
        Value::Object(map) => {
            for (k, val) in map {
                target.insert(k.clone(), val.clone());
            }
        }
        Value::Array(items) => {
            for (i, val) in items.iter().enumerate() {
                target.insert(i.to_string(), val.clone());
            }
        }
        Value::String(s) => {
            for (i, unit) in s.encode_utf16().enumerate() {
                target.insert(
                    i.to_string(),
                    Value::from(String::from_utf16_lossy(&[unit])),
                );
            }
        }
        _ => {}
    }
}

pub(crate) fn unique_strings(values: impl IntoIterator<Item = Value>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for value in values {
        if let Some(s) = value.as_str() {
            let trimmed = s.trim();
            if !trimmed.is_empty() && seen.insert(trimmed.to_string()) {
                out.push(trimmed.to_string());
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Path helpers (Node `path` module subset)
// ---------------------------------------------------------------------------

pub(crate) fn home_dir() -> Option<PathBuf> {
    // Mirror Node's os.homedir(): USERPROFILE wins on Windows, HOME elsewhere.
    if cfg!(windows) {
        std::env::var_os("USERPROFILE")
            .filter(|v| !v.is_empty())
            .or_else(|| std::env::var_os("HOME").filter(|v| !v.is_empty()))
    } else {
        std::env::var_os("HOME").filter(|v| !v.is_empty())
    }
    .map(PathBuf::from)
}

pub(crate) fn current_dir() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
}

fn split_path(value: &str) -> (bool, Vec<String>) {
    let win = cfg!(windows);
    let normalized: String = if win {
        value.replace('/', "\\")
    } else {
        value.to_string()
    };
    let absolute = if win {
        normalized.starts_with('\\')
            || Path::new(&normalized).is_absolute()
            || normalized.len() >= 2
                && normalized.as_bytes()[1] == b':'
                && normalized.as_bytes()[0].is_ascii_alphabetic()
    } else {
        normalized.starts_with('/')
    };
    let parts: Vec<String> = normalized
        .split(if win { '\\' } else { '/' })
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    (absolute, parts)
}

fn join_parts(absolute: bool, parts: &[String]) -> PathBuf {
    let win = cfg!(windows);
    if !absolute {
        return PathBuf::from(parts.join(if win { "\\" } else { "/" }));
    }
    if parts.is_empty() {
        return PathBuf::from(if win { "\\" } else { "/" });
    }
    if win {
        let joined = parts.join("\\");
        if joined.len() >= 2 && joined.as_bytes()[1] == b':' {
            if parts.len() == 1 {
                PathBuf::from(format!("{joined}\\"))
            } else {
                PathBuf::from(joined)
            }
        } else {
            PathBuf::from(format!("\\{joined}"))
        }
    } else {
        PathBuf::from(format!("/{}", parts.join("/")))
    }
}

/// JS `path.resolve(value)`: make the path absolute against the cwd and
/// lexically normalize `.`/`..` segments (no symlink resolution).
pub fn path_resolve(value: &str) -> PathBuf {
    let win = cfg!(windows);
    let (mut absolute, parts) = split_path(value);
    let mut stack: Vec<String> = Vec::new();
    if !absolute {
        let (cwd_abs, cwd_parts) = split_path(&current_dir().to_string_lossy());
        absolute = cwd_abs;
        stack.extend(cwd_parts);
    }
    for part in parts {
        match part.as_str() {
            "." => {}
            ".." => {
                let at_root = stack.is_empty()
                    || (win
                        && stack.len() == 1
                        && stack[0].len() == 2
                        && stack[0].as_bytes()[1] == b':');
                if !at_root {
                    stack.pop();
                }
            }
            other => stack.push(other.to_string()),
        }
    }
    // Windows drive letters normalize to uppercase (Node:
    // path.resolve("c:\\x") === "C:\\x"). Uppercase the leading drive in
    // place — pushing it separately before the part loop (as an earlier
    // revision did) duplicated the drive on every resolve, compounding a
    // "C:\\C:\\…" prefix into persisted paths once per settings save.
    if win
        && let Some(first) = stack.first_mut()
        && first.len() == 2
        && first.as_bytes()[1] == b':'
    {
        *first = first.to_uppercase();
    }
    let resolved = join_parts(absolute, &stack);
    if resolved.as_os_str().is_empty() {
        current_dir()
    } else {
        resolved
    }
}

/// JS `path.join(a, b)` for a relative `b`.
pub(crate) fn path_join(base: &Path, rest: &str) -> PathBuf {
    let mut out = base.to_path_buf();
    out.push(rest);
    out
}

/// JS `path.relative(from, to)` (both resolved absolute, POSIX-style walk).
pub fn path_relative(from: &str, to: &str) -> String {
    let win = cfg!(windows);
    let sep = if win { '\\' } else { '/' };
    let from_resolved = path_resolve(from);
    let to_resolved = path_resolve(to);
    let from_str = from_resolved
        .to_string_lossy()
        .replace('/', if win { "\\" } else { "/" });
    let to_str = to_resolved
        .to_string_lossy()
        .replace('/', if win { "\\" } else { "/" });

    // Different windows drives resolve to an absolute relative path.
    if win {
        let drive = |s: &str| s.get(0..2).map(|d| d.to_uppercase());
        if let (Some(a), Some(b)) = (drive(&from_str), drive(&to_str))
            && a != b
            && a.ends_with(':')
            && b.ends_with(':')
        {
            return to_str;
        }
    }

    let from_parts: Vec<&str> = from_str
        .split(sep)
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    let to_parts: Vec<&str> = to_str
        .split(sep)
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();

    let mut common = 0;
    while common < from_parts.len()
        && common < to_parts.len()
        && from_parts[common] == to_parts[common]
    {
        common += 1;
    }
    let mut segments: Vec<String> = Vec::new();
    for _ in common..from_parts.len() {
        segments.push("..".to_string());
    }
    for part in &to_parts[common..] {
        segments.push((*part).to_string());
    }
    segments.join(if win { "\\" } else { "/" })
}

// ---------------------------------------------------------------------------
// settings-normalization-runtime.js
// ---------------------------------------------------------------------------

/// `normalizeDirectoryPath`: trims, strips matching surrounding quotes,
/// expands `~` / `~/` against the home directory.
pub fn normalize_directory_path(value: &str) -> String {
    let mut trimmed = collapse_repeated_drive_prefixes(value.trim());
    let b = trimmed.as_bytes();
    if b.len() >= 2
        && ((b[0] == b'"' && b[b.len() - 1] == b'"') || (b[0] == b'\'' && b[b.len() - 1] == b'\''))
    {
        trimmed = trimmed[1..trimmed.len() - 1].trim().to_string();
    }
    if trimmed.is_empty() {
        return trimmed;
    }
    if let Some(home) = home_dir() {
        if trimmed == "~" {
            return home.to_string_lossy().into_owned();
        }
        if trimmed.starts_with("~/") || trimmed.starts_with("~\\") {
            return path_join(&home, &trimmed[2..])
                .to_string_lossy()
                .into_owned();
        }
    }
    trimmed
}

/// Repair the persisted `C:\C:\…\C:\Users\…` corruption: a drive prefix can
/// appear exactly once at the start of a Windows path, so any repetition is
/// damage (settings written by builds whose path resolve duplicated the
/// drive). Collapse the repeats to one; no-op on other platforms and shapes.
fn collapse_repeated_drive_prefixes(value: &str) -> String {
    if !cfg!(windows) {
        return value.to_string();
    }
    let is_drive_sep = |s: &str| -> bool {
        let b = s.as_bytes();
        b.len() >= 3
            && b[0].is_ascii_alphabetic()
            && b[1] == b':'
            && (b[2] == b'\\' || b[2] == b'/')
    };
    let mut out = value.to_string();
    while is_drive_sep(&out) {
        let rest = out[3..].to_string();
        if !is_drive_sep(&rest) {
            break;
        }
        out = rest;
    }
    out
}

/// `safeRealpathSync`: resolve symlinks, falling back to the original value.
pub fn safe_realpath(value: &str) -> String {
    match std::fs::canonicalize(value) {
        Ok(resolved) => strip_verbatim_prefix(resolved)
            .to_string_lossy()
            .into_owned(),
        Err(_) => value.to_string(),
    }
}

/// Rust `fs::canonicalize` returns `\\?\`-prefixed verbatim paths on Windows;
/// Node's `fs.realpathSync` (which this mirrors) does not. Strip the prefix
/// so persisted paths stay comparable with every other path string.
pub(crate) fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    let text = path.as_os_str().to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        PathBuf::from(rest.to_string())
    } else {
        path
    }
}

fn uppercase_drive_letter(path: &str) -> String {
    let bytes = path.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_lowercase() && bytes[1] == b':' {
        let mut out = path.to_string();
        out.replace_range(0..1, &bytes[0].to_ascii_uppercase().to_string());
        return out;
    }
    path.to_string()
}

/// `normalizePathForPersistence(value, { resolveRealpath })`. Returns the
/// normalized string, or the input value untouched when it is not a string.
pub fn normalize_path_for_persistence(value: &Value, resolve_realpath: bool) -> Value {
    let Some(raw) = value.as_str() else {
        return value.clone();
    };
    let normalized = normalize_directory_path(raw);
    let trimmed = normalized.trim().to_string();
    if trimmed.is_empty() {
        return Value::from(trimmed);
    }
    let is_windows = cfg!(windows);
    let case_normalized = if is_windows {
        uppercase_drive_letter(&trimmed)
    } else {
        trimmed
    };
    let resolved = if resolve_realpath {
        safe_realpath(&case_normalized)
    } else {
        case_normalized
    };
    let final_resolved = if is_windows {
        uppercase_drive_letter(&resolved)
    } else {
        resolved
    };
    if !is_windows {
        return Value::from(final_resolved);
    }
    Value::from(final_resolved.replace('/', "\\"))
}

/// Convenience: normalized string form (empty string when not a string path).
pub(crate) fn normalized_path_string(value: &Value, resolve_realpath: bool) -> String {
    match normalize_path_for_persistence(value, resolve_realpath) {
        Value::String(s) => s,
        _ => String::new(),
    }
}

pub(crate) fn are_string_arrays_equal(a: &[Value], b: &[Value]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b.iter()).all(|(x, y)| x == y)
}

/// `normalizeStringArray`: unique, non-empty strings, first occurrence order.
pub fn normalize_string_array(input: &Value) -> Vec<String> {
    let Some(items) = input.as_array() else {
        return Vec::new();
    };
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for item in items {
        if let Some(s) = item.as_str()
            && !s.is_empty()
            && seen.insert(s.to_string())
        {
            out.push(s.to_string());
        }
    }
    out
}

fn is_hex_color(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.first() != Some(&b'#') {
        return false;
    }
    let rest = &bytes[1..];
    (rest.len() == 3 || rest.len() == 6) && rest.iter().all(|b| b.is_ascii_hexdigit())
}

/// `sanitizeProjects`: `None` when the input is not an array (JS `undefined`).
pub fn sanitize_projects(input: Option<&Value>) -> Option<Vec<Value>> {
    let items = input?.as_array()?;

    let mut result = Vec::new();
    let mut seen_ids: HashSet<String> = HashSet::new();
    let mut seen_paths: HashSet<String> = HashSet::new();

    for entry in items {
        let Some(entry) = entry.as_object() else {
            continue;
        };

        let id = entry
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        let raw_path = entry
            .get("path")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        let resolved_path = if raw_path.is_empty() {
            String::new()
        } else {
            safe_realpath(&path_resolve(&normalize_directory_path(raw_path)).to_string_lossy())
        };
        let normalized_path = if resolved_path.is_empty() {
            String::new()
        } else {
            normalized_path_string(&Value::from(resolved_path.clone()), false)
        };
        let label = string_field_obj(entry, "label");
        let icon = string_field_obj(entry, "icon");
        let icon_background = entry
            .get("iconBackground")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .filter(|s| is_hex_color(s))
            .map(|s| s.to_lowercase());
        let color = string_field_obj(entry, "color");
        let default_model = string_field_obj(entry, "defaultModel");
        let default_variant = string_field_obj(entry, "defaultVariant");
        let added_at_raw = entry.get("addedAt");
        let added_at = finite_number(added_at_raw.unwrap_or(&Value::Null));
        let last_opened_at_raw = entry.get("lastOpenedAt");
        let last_opened_at = finite_number(last_opened_at_raw.unwrap_or(&Value::Null));

        if id.is_empty() || normalized_path.is_empty() {
            continue;
        }
        if !seen_ids.insert(id.to_string()) {
            continue;
        }
        if !seen_paths.insert(normalized_path.clone()) {
            continue;
        }

        let mut project = Map::new();
        project.insert("id".into(), Value::from(id));
        project.insert("path".into(), Value::from(normalized_path.clone()));
        if let Some(label) = label {
            project.insert("label".into(), Value::from(label));
        }
        if let Some(icon) = icon {
            project.insert("icon".into(), Value::from(icon));
        }
        if let Some(background) = icon_background.clone() {
            project.insert("iconBackground".into(), Value::from(background));
        }
        if let Some(color) = color {
            project.insert("color".into(), Value::from(color));
        }
        if let Some(model) = default_model.as_deref().filter(|m| m.contains('/')) {
            project.insert("defaultModel".into(), Value::from(model));
            if let Some(variant) = default_variant.filter(|v| !v.is_empty()) {
                project.insert("defaultVariant".into(), Value::from(variant));
            }
        }
        if let Some(added) = added_at.filter(|v| *v >= 0.0) {
            project.insert("addedAt".into(), num_to_json(added));
        }
        if let Some(opened) = last_opened_at.filter(|v| *v >= 0.0) {
            project.insert("lastOpenedAt".into(), num_to_json(opened));
        }

        match entry.get("iconImage") {
            Some(Value::Null) => {
                project.insert("iconImage".into(), Value::Null);
            }
            Some(icon_image @ (Value::Object(_) | Value::Array(_))) => {
                let mime = icon_image
                    .get("mime")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .unwrap_or_default();
                let updated_at = icon_image
                    .get("updatedAt")
                    .and_then(finite_number)
                    .map(|v| js_round(v.max(0.0)))
                    .unwrap_or(0.0);
                let source = icon_image
                    .get("source")
                    .and_then(Value::as_str)
                    .filter(|s| *s == "custom" || *s == "auto");
                if !mime.is_empty()
                    && updated_at > 0.0
                    && let Some(source) = source
                {
                    project.insert(
                            "iconImage".into(),
                            serde_json::json!({ "mime": mime, "updatedAt": num_to_json(updated_at), "source": source }),
                        );
                }
            }
            _ => {}
        }

        if entry.get("iconBackground") == Some(&Value::Null) {
            project.insert("iconBackground".into(), Value::Null);
        }

        if let Some(collapsed) = entry.get("sidebarCollapsed").and_then(Value::as_bool) {
            project.insert("sidebarCollapsed".into(), Value::from(collapsed));
        }

        result.push(Value::Object(project));
    }

    Some(result)
}

fn string_field_obj(entry: &Map<String, Value>, key: &str) -> Option<String> {
    entry
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// `normalizeSettingsPaths`: canonicalize `lastDirectory`, `homeDirectory`,
/// `pinnedDirectories`, and re-sanitize `projects`. Returns the (possibly
// unchanged) settings map plus a changed flag.
pub fn normalize_settings_paths(input: &Map<String, Value>) -> (Map<String, Value>, bool) {
    let mut next = input.clone();
    let mut changed = false;

    let mut normalize_path_field = |next: &mut Map<String, Value>, key: &str| {
        let Some(current) = next.get(key) else { return };
        if !current.is_string() || current.as_str().is_some_and(str::is_empty) {
            return;
        }
        let normalized = normalize_path_for_persistence(current, true);
        if &normalized != current {
            next.insert(key.to_string(), normalized);
            changed = true;
        }
    };
    normalize_path_field(&mut next, "lastDirectory");
    normalize_path_field(&mut next, "homeDirectory");

    if let Some(Value::Array(items)) = next.get("pinnedDirectories") {
        let mapped: Vec<Value> = items
            .iter()
            .map(|entry| {
                if entry.is_string() {
                    normalize_path_for_persistence(entry, true)
                } else {
                    entry.clone()
                }
            })
            .filter(|entry| entry.as_str().is_some_and(|s| !s.is_empty()))
            .collect();
        let normalized: Vec<Value> = normalize_string_array(&Value::Array(mapped))
            .into_iter()
            .map(Value::from)
            .collect();
        let current: Vec<Value> = items.clone();
        if !are_string_arrays_equal(&normalized, &current) {
            next.insert("pinnedDirectories".into(), Value::Array(normalized));
            changed = true;
        }
    }

    if let Some(projects) = next.get("projects")
        && projects.is_array()
    {
        let normalized = sanitize_projects(Some(projects)).unwrap_or_default();
        if Value::Array(normalized.clone()) != *projects {
            next.insert("projects".into(), Value::Array(normalized));
            changed = true;
        }
    }

    (next, changed)
}

/// `normalizeTunnelBootstrapTtlMs`: `Value::Null` for null, clamped integer
/// for finite numbers, default for anything else.
pub fn normalize_tunnel_bootstrap_ttl_ms(value: &Value) -> Value {
    if value.is_null() {
        return Value::Null;
    }
    match finite_number(value) {
        Some(v) => num_to_json(clamp_number(
            js_round(v),
            TUNNEL_BOOTSTRAP_TTL_MIN_MS,
            TUNNEL_BOOTSTRAP_TTL_MAX_MS,
        )),
        None => num_to_json(TUNNEL_BOOTSTRAP_TTL_DEFAULT_MS),
    }
}

pub fn normalize_tunnel_session_ttl_ms(value: &Value) -> Value {
    match finite_number(value) {
        Some(v) => num_to_json(clamp_number(
            js_round(v),
            TUNNEL_SESSION_TTL_MIN_MS,
            TUNNEL_SESSION_TTL_MAX_MS,
        )),
        None => num_to_json(TUNNEL_SESSION_TTL_DEFAULT_MS),
    }
}

/// `normalizeManagedRemoteTunnelHostname`: parse a bare hostname or URL and
/// return the lowercase host, or `None` (JS `undefined`).
pub fn normalize_managed_remote_tunnel_hostname(value: &Value) -> Option<String> {
    let raw = value.as_str()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let parsed = if trimmed.contains("://") {
        url::Url::parse(trimmed)
    } else {
        url::Url::parse(&format!("https://{trimmed}"))
    };
    let hostname = parsed
        .ok()
        .and_then(|u| u.host_str().map(|h| h.trim().to_lowercase()))
        .unwrap_or_default();
    if hostname.is_empty() {
        None
    } else {
        Some(hostname)
    }
}

/// `normalizeManagedRemoteTunnelPresets`: `Some` (possibly empty) for arrays.
pub fn normalize_managed_remote_tunnel_presets(value: &Value) -> Option<Vec<Value>> {
    let items = value.as_array()?;
    let mut result = Vec::new();
    let mut seen_ids: HashSet<String> = HashSet::new();
    let mut seen_hostnames: HashSet<String> = HashSet::new();
    for entry in items {
        let Some(entry) = entry.as_object() else {
            continue;
        };
        let Some(id) = string_field_obj(entry, "id") else {
            continue;
        };
        let Some(name) = string_field_obj(entry, "name") else {
            continue;
        };
        let Some(hostname) =
            normalize_managed_remote_tunnel_hostname(entry.get("hostname").unwrap_or(&Value::Null))
        else {
            continue;
        };
        if !seen_ids.insert(id.clone()) || !seen_hostnames.insert(hostname.clone()) {
            continue;
        }
        result.push(serde_json::json!({ "id": id, "name": name, "hostname": hostname }));
    }
    Some(result)
}

/// `normalizeManagedRemoteTunnelPresetTokens`: `Some` only when non-empty.
pub fn normalize_managed_remote_tunnel_preset_tokens(value: &Value) -> Option<Map<String, Value>> {
    let map = match value {
        Value::Object(map) => map,
        _ => return None,
    };
    let mut result = Map::new();
    for (raw_id, raw_token) in map {
        let id = raw_id.trim();
        let token = raw_token.as_str().map(str::trim).unwrap_or_default();
        if id.is_empty() || token.is_empty() {
            continue;
        }
        result.insert(id.to_string(), Value::from(token));
    }
    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}

/// `isUnsafeSkillRelativePath` (ported for completeness of the runtime).
pub fn is_unsafe_skill_relative_path(value: &str) -> bool {
    if value.is_empty() {
        return true;
    }
    let normalized = value.replace('\\', "/");
    if normalized.starts_with('/') {
        return true;
    }
    normalized.split('/').any(|segment| segment == "..")
}

/// `sanitizeTypographySizesPartial`: `Some` only when at least one known key
/// holds a non-empty string.
pub fn sanitize_typography_sizes_partial(input: Option<&Value>) -> Option<Map<String, Value>> {
    let candidate = match input {
        Some(v) if js_truthy(v) && (v.is_object() || v.is_array()) => v,
        _ => return None,
    };
    let mut result = Map::new();
    for key in ["markdown", "code", "uiHeader", "uiLabel", "meta", "micro"] {
        if let Some(s) = candidate.get(key).and_then(Value::as_str)
            && !s.is_empty()
        {
            result.insert(key.to_string(), Value::from(s));
        }
    }
    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}

/// `sanitizeModelRefs`: `Some` (possibly empty) for arrays.
pub fn sanitize_model_refs(input: &Value, limit: usize) -> Option<Vec<Value>> {
    let items = input.as_array()?;
    let mut result = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for entry in items {
        let Some(entry) = entry.as_object() else {
            continue;
        };
        let Some(provider_id) = string_field_obj(entry, "providerID") else {
            continue;
        };
        let Some(model_id) = string_field_obj(entry, "modelID") else {
            continue;
        };
        let key = format!("{provider_id}/{model_id}");
        if !seen.insert(key) {
            continue;
        }
        result.push(serde_json::json!({ "providerID": provider_id, "modelID": model_id }));
        if result.len() >= limit {
            break;
        }
    }
    Some(result)
}

/// `sanitizeSkillCatalogs`: `Some` (possibly empty) for arrays.
pub fn sanitize_skill_catalogs(input: &Value) -> Option<Vec<Value>> {
    let items = input.as_array()?;
    let mut result = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for entry in items {
        let Some(entry) = entry.as_object() else {
            continue;
        };
        let Some(id) = string_field_obj(entry, "id") else {
            continue;
        };
        let Some(label) = string_field_obj(entry, "label") else {
            continue;
        };
        let Some(source) = string_field_obj(entry, "source") else {
            continue;
        };
        let subpath = string_field_obj(entry, "subpath");
        let git_identity_id = string_field_obj(entry, "gitIdentityId");
        if !seen.insert(id.clone()) {
            continue;
        }
        let mut out = Map::new();
        out.insert("id".into(), Value::from(id));
        out.insert("label".into(), Value::from(label));
        out.insert("source".into(), Value::from(source));
        if let Some(subpath) = subpath {
            out.insert("subpath".into(), Value::from(subpath));
        }
        if let Some(git_identity_id) = git_identity_id {
            out.insert("gitIdentityId".into(), Value::from(git_identity_id));
        }
        result.push(Value::Object(out));
    }
    Some(result)
}

// ---------------------------------------------------------------------------
// lib/tunnels/types.js subset used by the settings helpers
// ---------------------------------------------------------------------------

pub fn normalize_tunnel_provider(value: &Value) -> String {
    let Some(raw) = value.as_str() else {
        return TUNNEL_PROVIDER_CLOUDFLARE.to_string();
    };
    let provider = raw.trim().to_lowercase();
    if provider.is_empty() || (provider != "cloudflare" && provider != "ngrok") {
        return TUNNEL_PROVIDER_CLOUDFLARE.to_string();
    }
    provider
}

pub fn normalize_tunnel_mode(value: &Value) -> String {
    let Some(raw) = value.as_str() else {
        return "quick".to_string();
    };
    let mode = raw.trim().to_lowercase();
    match mode.as_str() {
        "quick" | "managed-remote" | "managed-local" => mode,
        _ => "quick".to_string(),
    }
}

pub fn is_path_within_directory(candidate: &Path, directory: &Path) -> bool {
    let resolved_candidate = path_resolve(&candidate.to_string_lossy());
    let resolved_directory = path_resolve(&directory.to_string_lossy());
    let comparable = |p: &Path| {
        let s = p.to_string_lossy().to_string();
        if cfg!(windows) { s.to_lowercase() } else { s }
    };
    let candidate = comparable(&resolved_candidate);
    let directory = comparable(&resolved_directory);
    let sep = if cfg!(windows) { '\\' } else { '/' };
    candidate == directory || candidate.starts_with(&format!("{directory}{sep}"))
}

/// `resolveTunnelConfigPath`: `~` expansion, resolve, and home-dir
/// containment enforcement (throws `TunnelServiceError` in JS → error here).
fn resolve_tunnel_config_path(value: &str) -> Result<PathBuf, AppError> {
    let home = home_dir().ok_or_else(|| AppError::internal("home directory unavailable"))?;
    let resolved = if value == "~" {
        home.clone()
    } else if value.starts_with("~/") || value.starts_with("~\\") {
        path_join(&home, &value[2..])
    } else {
        path_resolve(value)
    };
    if !is_path_within_directory(&resolved, &home) {
        return Err(AppError::internal(format!(
            "Config path must be within the home directory ({}). Got: {}",
            home.to_string_lossy(),
            resolved.to_string_lossy()
        )));
    }
    Ok(resolved)
}

/// `normalizeOptionalPath` for a non-empty trimmed string input.
pub fn normalize_optional_path(value: &str) -> Result<String, AppError> {
    resolve_tunnel_config_path(value).map(|p| p.to_string_lossy().into_owned())
}

// ---------------------------------------------------------------------------
// lib/projects/project-id.js
// ---------------------------------------------------------------------------

/// `createProjectIdFromPath`: `path_<base64url(normalized path)>`.
pub fn create_project_id_from_path(project_path: &str) -> String {
    let normalized = {
        let replaced = project_path.replace('\\', "/");
        let trimmed_end = replaced.trim_end_matches('/');
        let trimmed = trimmed_end.trim();
        if trimmed.is_empty() {
            replaced.trim().to_string()
        } else {
            trimmed.to_string()
        }
    };
    if normalized.is_empty() {
        return String::new();
    }
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(normalized.as_bytes());
    format!("path_{encoded}")
}

// ---------------------------------------------------------------------------
// SHA-1 (JS: crypto.createHash('sha1')) — no sha1 crate is in Cargo.toml, so
// the digest is implemented here for project-icon file naming.
// ---------------------------------------------------------------------------

pub(crate) fn sha1_hex(value: &str) -> String {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let data = value.as_bytes();

    let ml = (data.len() as u64).wrapping_mul(8);
    let mut message = data.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&ml.to_be_bytes());

    let mut w = [0u32; 80];
    for chunk in message.chunks_exact(64) {
        for (i, word) in chunk.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }

    h.iter().map(|word| format!("{word:08x}")).collect()
}
