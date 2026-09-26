//! Port of `server/lib/opencode/settings-helpers.js`: update sanitization,
//! persisted merge, and response shaping for `/api/config/settings`.

use std::collections::{BTreeMap, HashSet};

use serde_json::{Map, Value};

use super::normalization::{
    clamp_number, finite_number, is_safe_integer, js_round, js_truthy,
    normalize_managed_remote_tunnel_hostname, normalize_managed_remote_tunnel_preset_tokens,
    normalize_managed_remote_tunnel_presets, normalize_optional_path,
    normalize_path_for_persistence, normalize_string_array, normalize_tunnel_bootstrap_ttl_ms,
    normalize_tunnel_mode, normalize_tunnel_provider, normalize_tunnel_session_ttl_ms, num_to_json,
    object_entries, sanitize_model_refs, sanitize_projects, sanitize_skill_catalogs,
    sanitize_typography_sizes_partial, spread_into,
};
use crate::error::AppError;

const PWA_APP_NAME_MAX_LENGTH: usize = 64;
const STT_SERVER_URL_MAX_LENGTH: usize = 2048;
const STT_MODEL_MAX_LENGTH: usize = 256;
const STT_LANGUAGE_MAX_LENGTH: usize = 64;
const SHORTCUT_OVERRIDE_KEY_MAX_LENGTH: usize = 128;
const SHORTCUT_OVERRIDE_VALUE_MAX_LENGTH: usize = 128;
const HIDDEN_MODELS_MAX: usize = 1024;
const RECENT_EFFORTS_MAX_KEYS: usize = 128;
const RECENT_EFFORTS_MAX_VARIANTS_PER_KEY: usize = 5;

/// Sanitized update: `None` values model JS `undefined` assignments — the key
/// was present in the update, so it CLEARS any persisted value (and is
/// dropped by JSON serialization, exactly like `JSON.stringify`).
pub type SanitizedUpdate = BTreeMap<String, Option<Value>>;

fn one_of(value: &str, allowed: &[&str]) -> Option<String> {
    allowed
        .iter()
        .find(|allowed| **allowed == value)
        .map(|v| v.to_string())
}

// ---------------------------------------------------------------------------
// settings-helpers.js small normalizers
// ---------------------------------------------------------------------------

pub fn normalize_pwa_app_name(value: &Value, fallback: &str) -> String {
    let Some(raw) = value.as_str() else {
        return fallback.to_string();
    };
    let mut normalized = String::new();
    let mut last_was_space = true;
    for ch in raw.trim().chars() {
        // JS regex `/\s+/g` → single ASCII space (includes U+FEFF).
        if ch.is_whitespace() || ch == '\u{feff}' {
            if !last_was_space {
                normalized.push(' ');
                last_was_space = true;
            }
        } else {
            normalized.push(ch);
            last_was_space = false;
        }
    }
    let collapsed = normalized.trim_end().to_string();
    if collapsed.is_empty() {
        return fallback.to_string();
    }
    collapsed.chars().take(PWA_APP_NAME_MAX_LENGTH).collect()
}

pub fn normalize_pwa_orientation(value: &Value, fallback: &str) -> String {
    match value.as_str().map(str::trim) {
        Some(v) if matches!(v, "system" | "portrait" | "landscape") => v.to_string(),
        _ => fallback.to_string(),
    }
}

pub fn normalize_mobile_keyboard_mode(value: &Value, fallback: &str) -> String {
    match value.as_str().map(str::trim) {
        Some(v) if matches!(v, "native" | "resize-content") => v.to_string(),
        _ => fallback.to_string(),
    }
}

/// `normalizeFollowUpBehavior(value, legacyQueueModeEnabled)` — `None` mirrors
/// the JS `null` default argument.
pub fn normalize_follow_up_behavior(
    value: Option<&Value>,
    legacy_queue_mode_enabled: Option<bool>,
) -> String {
    if value.and_then(Value::as_str) == Some("immediate") {
        return "steer".to_string();
    }
    if let Some(v) = value.and_then(Value::as_str)
        && (v == "steer" || v == "queue")
    {
        return v.to_string();
    }
    if legacy_queue_mode_enabled == Some(false) {
        return "steer".to_string();
    }
    "queue".to_string()
}

fn sanitize_shortcut_overrides(value: &Value) -> Option<Map<String, Value>> {
    let map = match value {
        Value::Object(map) => map,
        Value::Null => return None,
        _ if value.as_array().is_some() => return None,
        _ if value.as_str().is_some() || value.as_f64().is_some() || value.as_bool().is_some() => {
            return None;
        }
        _ => return None,
    };
    let mut result = Map::new();
    for (raw_key, raw_value) in map {
        let key = raw_key.trim();
        let combo = raw_value.as_str().map(str::trim).unwrap_or_default();
        if key.is_empty() || combo.is_empty() {
            continue;
        }
        result.insert(
            key.chars().take(SHORTCUT_OVERRIDE_KEY_MAX_LENGTH).collect(),
            Value::from(
                combo
                    .chars()
                    .take(SHORTCUT_OVERRIDE_VALUE_MAX_LENGTH)
                    .collect::<String>(),
            ),
        );
    }
    Some(result)
}

fn sanitize_recent_efforts(value: &Value) -> Option<Map<String, Value>> {
    let map = match value {
        Value::Object(map) => map,
        _ => return None,
    };
    let mut result = Map::new();
    let mut seen_keys: HashSet<String> = HashSet::new();
    let mut count = 0usize;
    for (raw_key, raw_variants) in map {
        let key = raw_key.trim();
        if key.is_empty() || seen_keys.contains(key) {
            continue;
        }
        let Some(variants_in) = raw_variants.as_array() else {
            continue;
        };
        let mut variants: Vec<Value> = Vec::new();
        let mut seen_variants: HashSet<String> = HashSet::new();
        for raw_variant in variants_in {
            let variant = raw_variant.as_str().map(str::trim).unwrap_or_default();
            if variant.is_empty() || seen_variants.contains(variant) {
                continue;
            }
            seen_variants.insert(variant.to_string());
            variants.push(Value::from(variant));
            if variants.len() >= RECENT_EFFORTS_MAX_VARIANTS_PER_KEY {
                break;
            }
        }
        if variants.is_empty() {
            continue;
        }
        seen_keys.insert(key.to_string());
        result.insert(key.to_string(), Value::Array(variants));
        count += 1;
        if count >= RECENT_EFFORTS_MAX_KEYS {
            break;
        }
    }
    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}

// ---------------------------------------------------------------------------
// sanitizeSettingsUpdate
// ---------------------------------------------------------------------------

/// Port of `sanitizeSettingsUpdate(payload)`. Returns the subset of recognized
/// settings keys with valid shapes. Errors only where the JS throws (a
/// managed tunnel config path outside the home directory).
pub fn sanitize_settings_update(payload: &Value) -> Result<SanitizedUpdate, AppError> {
    let mut result: SanitizedUpdate = BTreeMap::new();
    let candidate: &Map<String, Value> = match payload {
        Value::Object(map) => map,
        // JS: arrays pass the `typeof === 'object'` gate; every field lookup
        // then misses and the result is empty.
        Value::Array(_) => return Ok(result),
        _ => return Ok(result),
    };
    let get = |key: &str| candidate.get(key);
    let set = |result: &mut SanitizedUpdate, key: &str, value: Option<Value>| {
        result.insert(key.to_string(), value);
    };

    // --- theme --------------------------------------------------------------
    if let Some(v) = get("themeId").filter(|v| v.as_str().is_some_and(|s| !s.is_empty())) {
        set(&mut result, "themeId", Some(v.clone()));
    }
    if let Some(v) =
        get("themeVariant").filter(|v| matches!(v.as_str(), Some("light") | Some("dark")))
    {
        set(&mut result, "themeVariant", Some(v.clone()));
    }
    if let Some(v) = get("useSystemTheme").and_then(Value::as_bool) {
        set(&mut result, "useSystemTheme", Some(Value::from(v)));
    }
    for key in ["lightThemeId", "darkThemeId"] {
        if let Some(v) = get(key).filter(|v| v.as_str().is_some_and(|s| !s.is_empty())) {
            set(&mut result, key, Some(v.clone()));
        }
    }
    for key in [
        "splashBgLight",
        "splashFgLight",
        "splashBgDark",
        "splashFgDark",
    ] {
        if let Some(v) = get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            set(&mut result, key, Some(Value::from(v)));
        }
    }
    // --- directories --------------------------------------------------------
    for key in ["lastDirectory", "homeDirectory"] {
        if let Some(v) = get(key).filter(|v| v.as_str().is_some_and(|s| !s.is_empty())) {
            let normalized = normalize_path_for_persistence(v, true);
            if normalized.as_str().is_some_and(|s| !s.is_empty()) {
                set(&mut result, key, Some(normalized));
            }
        }
    }
    // --- work status --------------------------------------------------------
    if let Some(v) = get("workStatusPanelEnabled").and_then(Value::as_bool) {
        set(&mut result, "workStatusPanelEnabled", Some(Value::from(v)));
    }
    if let Some(items) = get("workStatusHiddenSections").and_then(Value::as_array) {
        let unique: Vec<Value> = normalize_string_array(&Value::Array(items.clone()))
            .into_iter()
            .map(Value::from)
            .collect();
        set(
            &mut result,
            "workStatusHiddenSections",
            Some(Value::Array(unique)),
        );
    }
    // --- desktop ------------------------------------------------------------
    for key in [
        "desktopLanAccessEnabled",
        "desktopKeepAwakeEnabled",
        "desktopMinimizeToTrayEnabled",
        "desktopMacMenuBarEnabled",
    ] {
        if let Some(v) = get(key).and_then(Value::as_bool) {
            set(&mut result, key, Some(Value::from(v)));
        }
    }
    if let Some(mode) = get("desktopWindowControlsPosition")
        .and_then(Value::as_str)
        .map(str::trim)
    {
        // Legacy "auto" never read OS chrome config; persist as the default.
        if mode == "auto" || mode == "right" {
            set(
                &mut result,
                "desktopWindowControlsPosition",
                Some(Value::from("right")),
            );
        } else if mode == "left" {
            set(
                &mut result,
                "desktopWindowControlsPosition",
                Some(Value::from("left")),
            );
        }
    }
    if let Some(style) = get("desktopWindowControlsStyle")
        .and_then(Value::as_str)
        .map(str::trim)
        && (style == "classic" || style == "traffic-lights")
    {
        set(
            &mut result,
            "desktopWindowControlsStyle",
            Some(Value::from(style)),
        );
    }
    // --- permission auto-accept ----------------------------------------------
    if let Some(policy) = get("permissionAutoAccept").and_then(Value::as_object) {
        let mut sessions = Map::new();
        if let Some(entries) = policy.get("sessions").and_then(Value::as_object) {
            for (session_id, enabled) in entries {
                if let Some(enabled) = enabled.as_bool()
                    && !session_id.is_empty()
                {
                    sessions.insert(session_id.clone(), Value::from(enabled));
                }
            }
        }
        let revision = policy
            .get("revision")
            .and_then(is_safe_integer)
            .filter(|r| *r >= 0)
            .unwrap_or(0);
        set(
            &mut result,
            "permissionAutoAccept",
            Some(serde_json::json!({ "sessions": sessions, "revision": revision })),
        );
    }
    if let Some(password) = get("desktopUiPassword").and_then(Value::as_str) {
        set(
            &mut result,
            "desktopUiPassword",
            Some(Value::from(password.trim())),
        );
    }
    // --- projects / sidebar --------------------------------------------------
    if let Some(projects_in) = get("projects").filter(|v| v.is_array())
        && let Some(projects) = sanitize_projects(Some(projects_in))
    {
        set(&mut result, "projects", Some(Value::Array(projects)));
    }
    if let Some(v) = get("activeProjectId").filter(|v| v.as_str().is_some_and(|s| !s.is_empty())) {
        set(&mut result, "activeProjectId", Some(v.clone()));
    }
    for (key, allowed) in [
        ("sidebarProjectDisplayMode", &["all", "single"][..]),
        ("sidebarSessionGroupingMode", &["by-worktree", "flat"][..]),
        (
            "sidebarProjectSortOrder",
            &["manual", "a-z", "z-a", "date-added", "recent"][..],
        ),
    ] {
        if let Some(v) = get(key)
            .and_then(Value::as_str)
            .and_then(|v| one_of(v, allowed))
        {
            set(&mut result, key, Some(Value::from(v)));
        }
    }
    if let Some(v) = get("sidebarShowRecentSection").and_then(Value::as_bool) {
        set(
            &mut result,
            "sidebarShowRecentSection",
            Some(Value::from(v)),
        );
    }
    if let Some(items) = get("securityScopedBookmarks").filter(|v| v.is_array()) {
        let unique: Vec<Value> = normalize_string_array(items)
            .into_iter()
            .map(Value::from)
            .collect();
        set(
            &mut result,
            "securityScopedBookmarks",
            Some(Value::Array(unique)),
        );
    }
    if let Some(items) = get("pinnedDirectories").and_then(Value::as_array) {
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
        let unique: Vec<Value> = normalize_string_array(&Value::Array(mapped))
            .into_iter()
            .map(Value::from)
            .collect();
        set(&mut result, "pinnedDirectories", Some(Value::Array(unique)));
    }
    // --- draft starters ------------------------------------------------------
    if let Some(items) = get("draftStarters").and_then(Value::as_array) {
        let mut seen: HashSet<String> = HashSet::new();
        let mut starters = Vec::new();
        for entry in items {
            let Some(entry) = entry.as_object() else {
                continue;
            };
            let starter_type = match entry.get("type").and_then(Value::as_str) {
                Some("command") => "command",
                Some("skill") => "skill",
                _ => continue,
            };
            let name = entry
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            let key = format!("{starter_type}:{name}");
            if !seen.insert(key) {
                continue;
            }
            starters.push(serde_json::json!({ "type": starter_type, "name": name }));
        }
        set(&mut result, "draftStarters", Some(Value::Array(starters)));
    }
    for key in [
        "draftStartersVisible",
        "draftStartersCraftGoalAdded",
        "draftStartersScheduleTaskAdded",
    ] {
        if let Some(v) = get(key).and_then(Value::as_bool) {
            set(&mut result, key, Some(Value::from(v)));
        }
    }
    // --- fonts / editor ------------------------------------------------------
    for key in ["uiFont", "monoFont", "markdownDisplayMode"] {
        if let Some(v) = get(key).filter(|v| v.as_str().is_some_and(|s| !s.is_empty())) {
            set(&mut result, key, Some(v.clone()));
        }
    }
    for key in ["githubClientId", "githubScopes"] {
        if let Some(v) = get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            set(&mut result, key, Some(Value::from(v)));
        }
    }
    // --- booleans (sessions / rendering / tools) -----------------------------
    for key in [
        "showReasoningTraces",
        "sessionRecapEnabled",
        "sessionSuggestionEnabled",
        "sessionGoalEnabled",
        "sessionGoalDefaultBudgetEnabled",
        "collapsibleThinkingBlocks",
        "showTextJustificationActivity",
        "showDeletionDialog",
        "nativeNotificationsEnabled",
        "notifyOnSubtasks",
        "notifyOnCompletion",
        "notifyOnError",
        "notifyOnQuestion",
        "summarizeLastMessage",
        "autoDeleteEnabled",
        "smallModelUseDefault",
        "autoCreateWorktree",
        "gitmojiEnabled",
        "defaultFileViewerPreview",
        "inputSpellcheckEnabled",
        "agentWebToolEnabled",
        "agentControlToolEnabled",
        "agentMemoryToolEnabled",
        "showToolFileIcons",
        "showTurnChangedFiles",
        "showExpandedBashTools",
        "showExpandedEditTools",
        "collapsibleUserMessages",
        "stickyUserHeader",
        "promptNavigatorEnabled",
        "expandedEditorToolbar",
        "wideChatLayoutEnabled",
        "showSplitAssistantMessageActions",
        "directoryShowHidden",
        "filesViewShowGitignored",
        "reportUsage",
        "responseStyleEnabled",
        "dictationEnabled",
    ] {
        if let Some(v) = get(key).and_then(Value::as_bool) {
            set(&mut result, key, Some(Value::from(v)));
        }
    }
    if let Some(session_budget) = get("sessionGoalDefaultBudget")
        .and_then(finite_number)
        .filter(|v| *v > 0.0)
    {
        set(
            &mut result,
            "sessionGoalDefaultBudget",
            Some(num_to_json(session_budget.floor())),
        );
    }
    if let Some(mode) = get("notificationMode")
        .and_then(Value::as_str)
        .map(str::trim)
        && (mode == "always" || mode == "hidden-only")
    {
        set(&mut result, "notificationMode", Some(Value::from(mode)));
    }
    if let Some(v) =
        get("notificationTemplates").filter(|v| js_truthy(v) && (v.is_object() || v.is_array()))
    {
        set(&mut result, "notificationTemplates", Some(v.clone()));
    }
    if let Some(v) = get("summaryThreshold").and_then(finite_number) {
        set(
            &mut result,
            "summaryThreshold",
            Some(num_to_json(js_round(v).max(0.0))),
        );
    }
    if let Some(v) = get("summaryLength").and_then(finite_number) {
        set(
            &mut result,
            "summaryLength",
            Some(num_to_json(js_round(v).max(10.0))),
        );
    }
    if let Some(v) = get("maxLastMessageLength").and_then(finite_number) {
        set(
            &mut result,
            "maxLastMessageLength",
            Some(num_to_json(js_round(v).max(10.0))),
        );
    }
    if let Some(v) = get("usageDisplayMode")
        .and_then(Value::as_str)
        .and_then(|v| one_of(v, &["usage", "remaining"]))
    {
        set(&mut result, "usageDisplayMode", Some(Value::from(v)));
    }
    if let Some(items) = get("usageDropdownProviders").filter(|v| v.is_array()) {
        let unique: Vec<Value> = normalize_string_array(items)
            .into_iter()
            .map(Value::from)
            .collect();
        set(
            &mut result,
            "usageDropdownProviders",
            Some(Value::Array(unique)),
        );
    }
    if let Some(v) = get("autoDeleteAfterDays").and_then(finite_number) {
        set(
            &mut result,
            "autoDeleteAfterDays",
            Some(num_to_json(clamp_number(js_round(v), 1.0, 365.0))),
        );
    }
    if let Some(v) = get("sessionRetentionAction")
        .and_then(Value::as_str)
        .and_then(|v| one_of(v, &["archive", "delete"]))
    {
        set(&mut result, "sessionRetentionAction", Some(Value::from(v)));
    }
    // --- tunnels -------------------------------------------------------------
    match get("tunnelBootstrapTtlMs") {
        Some(Value::Null) => set(&mut result, "tunnelBootstrapTtlMs", Some(Value::Null)),
        Some(v) if v.as_f64().is_some_and(f64::is_finite) => {
            set(
                &mut result,
                "tunnelBootstrapTtlMs",
                Some(normalize_tunnel_bootstrap_ttl_ms(v)),
            );
        }
        _ => {}
    }
    if let Some(v) = get("tunnelSessionTtlMs").filter(|v| v.as_f64().is_some_and(f64::is_finite)) {
        set(
            &mut result,
            "tunnelSessionTtlMs",
            Some(normalize_tunnel_session_ttl_ms(v)),
        );
    }
    if let Some(v) = get("tunnelProvider").filter(|v| v.as_str().is_some()) {
        set(
            &mut result,
            "tunnelProvider",
            Some(Value::from(normalize_tunnel_provider(v))),
        );
    }
    if let Some(v) = get("tunnelMode").filter(|v| v.as_str().is_some()) {
        set(
            &mut result,
            "tunnelMode",
            Some(Value::from(normalize_tunnel_mode(v))),
        );
    }
    match get("managedLocalTunnelConfigPath") {
        Some(Value::Null) => set(
            &mut result,
            "managedLocalTunnelConfigPath",
            Some(Value::Null),
        ),
        Some(Value::String(raw)) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                set(
                    &mut result,
                    "managedLocalTunnelConfigPath",
                    Some(Value::Null),
                );
            } else {
                // Mirrors the JS throw (TunnelServiceError) when the resolved
                // path is outside the home directory.
                set(
                    &mut result,
                    "managedLocalTunnelConfigPath",
                    Some(Value::from(normalize_optional_path(trimmed)?)),
                );
            }
        }
        _ => {}
    }
    if let Some(v) = get("managedRemoteTunnelHostname").filter(|v| v.as_str().is_some()) {
        set(
            &mut result,
            "managedRemoteTunnelHostname",
            normalize_managed_remote_tunnel_hostname(v).map(Value::from),
        );
    }
    match get("managedRemoteTunnelToken") {
        Some(Value::Null) => set(&mut result, "managedRemoteTunnelToken", Some(Value::Null)),
        Some(Value::String(raw)) => set(
            &mut result,
            "managedRemoteTunnelToken",
            Some(Value::from(raw.trim())),
        ),
        _ => {}
    }
    if let Some(presets) = normalize_managed_remote_tunnel_presets(
        get("managedRemoteTunnelPresets").unwrap_or(&Value::Null),
    ) {
        set(
            &mut result,
            "managedRemoteTunnelPresets",
            Some(Value::Array(presets)),
        );
    }
    if let Some(tokens) = normalize_managed_remote_tunnel_preset_tokens(
        get("managedRemoteTunnelPresetTokens").unwrap_or(&Value::Null),
    ) {
        set(
            &mut result,
            "managedRemoteTunnelPresetTokens",
            Some(Value::Object(tokens)),
        );
    }
    if let Some(v) = get("managedRemoteTunnelSelectedPresetId").and_then(Value::as_str) {
        let id = v.trim();
        set(
            &mut result,
            "managedRemoteTunnelSelectedPresetId",
            if id.is_empty() {
                None
            } else {
                Some(Value::from(id))
            },
        );
    }
    if let Some(typography) = sanitize_typography_sizes_partial(get("typographySizes")) {
        set(
            &mut result,
            "typographySizes",
            Some(Value::Object(typography)),
        );
    }
    // --- model selection -----------------------------------------------------
    for key in [
        "defaultModel",
        "defaultVariant",
        "defaultAgent",
        "smallModelOverride",
        "walkthroughModelOverride",
        "defaultGitIdentityId",
        "zenModel",
        "gitProviderId",
        "gitModelId",
    ] {
        if let Some(v) = get(key).and_then(Value::as_str) {
            let trimmed = v.trim();
            set(
                &mut result,
                key,
                if trimmed.is_empty() {
                    None
                } else {
                    Some(Value::from(trimmed))
                },
            );
        }
    }
    if let Some(behavior) = get("followUpBehavior").filter(|v| v.as_str().is_some()) {
        set(
            &mut result,
            "followUpBehavior",
            Some(Value::from(normalize_follow_up_behavior(
                Some(behavior),
                None,
            ))),
        );
    } else if let Some(queue_mode) = get("queueModeEnabled").and_then(Value::as_bool) {
        set(
            &mut result,
            "followUpBehavior",
            Some(Value::from(normalize_follow_up_behavior(
                None,
                Some(queue_mode),
            ))),
        );
    }
    // --- pwa / mobile --------------------------------------------------------
    if let Some(v) = get("pwaAppName").filter(|v| v.as_str().is_some()) {
        let normalized = normalize_pwa_app_name(v, "");
        set(
            &mut result,
            "pwaAppName",
            if normalized.is_empty() {
                None
            } else {
                Some(Value::from(normalized))
            },
        );
    }
    if let Some(v) = get("pwaOrientation").filter(|v| v.as_str().is_some()) {
        let normalized = normalize_pwa_orientation(v, "");
        set(
            &mut result,
            "pwaOrientation",
            if normalized.is_empty() {
                None
            } else {
                Some(Value::from(normalized))
            },
        );
    }
    if let Some(v) = get("mobileKeyboardMode").filter(|v| v.as_str().is_some()) {
        let mode = normalize_mobile_keyboard_mode(v, "");
        if !mode.is_empty() {
            set(&mut result, "mobileKeyboardMode", Some(Value::from(mode)));
        }
    }
    // --- chat rendering enums ------------------------------------------------
    for (key, allowed) in [
        (
            "toolCallExpansion",
            &["collapsed", "activity", "detailed", "changes"][..],
        ),
        ("timeFormatPreference", &["auto", "12h", "24h"][..]),
        ("weekStartPreference", &["auto", "sunday", "monday"][..]),
        ("chatRenderMode", &["sorted", "live"][..]),
        ("messageStreamTransport", &["auto", "ws", "sse"][..]),
        ("activityRenderMode", &["collapsed", "summary"][..]),
        ("mermaidRenderingMode", &["svg", "ascii"][..]),
        ("userMessageRenderingMode", &["markdown", "plain"][..]),
        (
            "diffLayoutPreference",
            &["dynamic", "inline", "side-by-side"][..],
        ),
        ("gitChangesViewMode", &["flat", "tree"][..]),
    ] {
        if let Some(v) = get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .and_then(|v| one_of(v, allowed))
        {
            set(&mut result, key, Some(Value::from(v)));
        }
    }
    if let Some(v) = get("openInAppId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        set(&mut result, "openInAppId", Some(Value::from(v)));
    }
    // --- sizes ---------------------------------------------------------------
    for (key, min, max) in [
        ("fontSize", 50.0, 200.0),
        ("terminalFontSize", 9.0, 52.0),
        ("editorFontSize", 9.0, 32.0),
        ("padding", 50.0, 200.0),
        ("cornerRadius", 0.0, 32.0),
        ("inputBarOffset", 0.0, 100.0),
    ] {
        if let Some(v) = get(key).and_then(finite_number) {
            set(
                &mut result,
                key,
                Some(num_to_json(clamp_number(js_round(v), min, max))),
            );
        }
    }
    if let Some(message_limit) = get("messageLimit").and_then(finite_number) {
        set(
            &mut result,
            "messageLimit",
            Some(num_to_json(clamp_number(
                js_round(message_limit),
                10.0,
                500.0,
            ))),
        );
    }
    if let Some(shell) = get("terminalShell").and_then(Value::as_str) {
        let shell = shell.trim().to_lowercase();
        if is_terminal_shell(&shell) {
            set(&mut result, "terminalShell", Some(Value::from(shell)));
        }
    }
    if let Some(items) = get("terminalLoginShells").and_then(Value::as_array) {
        let mut seen: HashSet<String> = HashSet::new();
        let mut shells = Vec::new();
        for item in items {
            if let Some(shell) = item.as_str() {
                let shell = shell.trim().to_lowercase();
                if is_terminal_shell(&shell) && seen.insert(shell.clone()) {
                    shells.push(Value::from(shell));
                }
            }
        }
        set(
            &mut result,
            "terminalLoginShells",
            Some(Value::Array(shells)),
        );
    }
    // --- shortcuts / model lists ---------------------------------------------
    if let Some(overrides) =
        sanitize_shortcut_overrides(get("shortcutOverrides").unwrap_or(&Value::Null))
    {
        set(
            &mut result,
            "shortcutOverrides",
            Some(Value::Object(overrides)),
        );
    }
    for (key, limit) in [
        ("favoriteModels", 64usize),
        ("recentModels", 16),
        ("hiddenModels", HIDDEN_MODELS_MAX),
    ] {
        if let Some(refs) = sanitize_model_refs(get(key).unwrap_or(&Value::Null), limit) {
            set(&mut result, key, Some(Value::Array(refs)));
        }
    }
    for key in ["collapsedModelProviders", "recentAgents"] {
        if let Some(items) = get(key).filter(|v| v.is_array()) {
            let unique: Vec<Value> = normalize_string_array(items)
                .into_iter()
                .map(Value::from)
                .collect();
            set(&mut result, key, Some(Value::Array(unique)));
        }
    }
    if let Some(efforts) = sanitize_recent_efforts(get("recentEfforts").unwrap_or(&Value::Null)) {
        set(&mut result, "recentEfforts", Some(Value::Object(efforts)));
    }
    if let Some(catalogs) = sanitize_skill_catalogs(get("skillCatalogs").unwrap_or(&Value::Null)) {
        set(&mut result, "skillCatalogs", Some(Value::Array(catalogs)));
    }
    // --- usage maps ----------------------------------------------------------
    for key in [
        "usageSelectedModels",
        "usageCollapsedFamilies",
        "usageExpandedFamilies",
    ] {
        if let Some(source) = get(key).filter(|v| js_truthy(v))
            && let Some(entries) = object_entries(source)
        {
            let mut sanitized = Map::new();
            for (provider_id, models) in entries {
                if provider_id.is_empty() {
                    continue;
                }
                if let Some(models) = models.as_array() {
                    let valid: Vec<Value> = models
                        .iter()
                        .filter(|m| m.as_str().is_some_and(|s| !s.is_empty()))
                        .cloned()
                        .collect();
                    if !valid.is_empty() {
                        sanitized.insert(provider_id, Value::Array(valid));
                    }
                }
            }
            if !sanitized.is_empty() {
                set(&mut result, key, Some(Value::Object(sanitized)));
            }
        }
    }
    if let Some(groups) = get("usageModelGroups").filter(|v| js_truthy(v))
        && let Some(entries) = object_entries(groups)
    {
        let mut sanitized = Map::new();
        for (provider_id, config) in entries {
            if provider_id.is_empty() {
                continue;
            }
            let mut provider_config = Map::new();
            if let Some(custom_groups) = config.get("customGroups").and_then(Value::as_array) {
                let valid: Vec<Value> = custom_groups
                    .iter()
                    .filter_map(|g| {
                        let id = g.get("id")?.as_str()?;
                        let label = g.get("label")?.as_str()?;
                        let models: Vec<Value> = g
                            .get("models")
                            .and_then(Value::as_array)
                            .map(|models| {
                                models
                                    .iter()
                                    .filter(|m| m.as_str().is_some())
                                    .cloned()
                                    .collect()
                            })
                            .unwrap_or_default();
                        Some(serde_json::json!({
                            "id": id.chars().take(64).collect::<String>(),
                            "label": label.chars().take(128).collect::<String>(),
                            "models": models.into_iter().take(500).collect::<Vec<_>>(),
                            "order": g.get("order").and_then(Value::as_f64).unwrap_or(0.0),
                        }))
                    })
                    .collect();
                if !valid.is_empty() {
                    provider_config.insert("customGroups".into(), Value::Array(valid));
                }
            }
            if let Some(assignments) = config.get("modelAssignments").filter(|v| js_truthy(v))
                && let Some(entries) = object_entries(assignments)
            {
                let mut valid = Map::new();
                for (model, group_id) in entries {
                    if model.is_empty() {
                        continue;
                    }
                    if let Some(group_id) = group_id.as_str() {
                        valid.insert(model, Value::from(group_id));
                    }
                }
                if !valid.is_empty() {
                    provider_config.insert("modelAssignments".into(), Value::Object(valid));
                }
            }
            if let Some(renamed) = config.get("renamedGroups").filter(|v| js_truthy(v))
                && let Some(entries) = object_entries(renamed)
            {
                let mut valid = Map::new();
                for (group_id, label) in entries {
                    if group_id.is_empty() {
                        continue;
                    }
                    if let Some(label) = label.as_str() {
                        valid.insert(
                            group_id,
                            Value::from(label.chars().take(128).collect::<String>()),
                        );
                    }
                }
                if !valid.is_empty() {
                    provider_config.insert("renamedGroups".into(), Value::Object(valid));
                }
            }
            if !provider_config.is_empty() {
                sanitized.insert(provider_id, Value::Object(provider_config));
            }
        }
        if !sanitized.is_empty() {
            set(
                &mut result,
                "usageModelGroups",
                Some(Value::Object(sanitized)),
            );
        }
    }
    // --- behavior prompt / response style ------------------------------------
    if let Some(prompt) = get("globalBehaviorPrompt").and_then(Value::as_str)
        && prompt.encode_utf16().count() <= 1024 * 1024
    {
        set(
            &mut result,
            "globalBehaviorPrompt",
            Some(Value::from(prompt)),
        );
    }
    if let Some(preset) = get("responseStylePreset")
        .and_then(Value::as_str)
        .and_then(|v| {
            one_of(
                v,
                &[
                    "concise",
                    "detailed",
                    "mentor",
                    "pushback",
                    "noFiller",
                    "matchEnergy",
                    "warmPeer",
                    "custom",
                ],
            )
        })
    {
        set(
            &mut result,
            "responseStylePreset",
            Some(Value::from(preset)),
        );
    }
    if let Some(instructions) = get("responseStyleCustomInstructions").and_then(Value::as_str)
        && instructions.encode_utf16().count() <= 50_000
    {
        set(
            &mut result,
            "responseStyleCustomInstructions",
            Some(Value::from(instructions)),
        );
    }
    // --- dictation / STT -----------------------------------------------------
    if let Some(provider) = get("sttProvider").and_then(Value::as_str).map(str::trim) {
        match provider {
            "local" | "openai-compatible" => {
                set(&mut result, "sttProvider", Some(Value::from(provider)))
            }
            // Legacy provider migrations.
            "server" => set(
                &mut result,
                "sttProvider",
                Some(Value::from("openai-compatible")),
            ),
            "browser" | "wasm" => set(&mut result, "sttProvider", Some(Value::from("local"))),
            _ => {}
        }
    }
    for (key, max) in [
        ("sttServerUrl", STT_SERVER_URL_MAX_LENGTH),
        ("sttModel", STT_MODEL_MAX_LENGTH),
        ("sttLocalModel", STT_MODEL_MAX_LENGTH),
        ("sttLanguage", STT_LANGUAGE_MAX_LENGTH),
    ] {
        if let Some(v) = get(key).and_then(Value::as_str).map(str::trim)
            && v.encode_utf16().count() <= max
        {
            set(&mut result, key, Some(Value::from(v)));
        }
    }

    Ok(result)
}

fn is_terminal_shell(shell: &str) -> bool {
    matches!(
        shell,
        "auto"
            | "bash"
            | "zsh"
            | "sh"
            | "fish"
            | "pwsh"
            | "powershell"
            | "cmd"
            | "dash"
            | "ksh"
            | "nu"
    )
}

// ---------------------------------------------------------------------------
// mergePersistedSettings
// ---------------------------------------------------------------------------

/// Port of `mergePersistedSettings(current, changes)`:
/// spread `current`, apply the sanitized update (`None` clears the key),
/// then re-materialize `securityScopedBookmarks` (always present, unique)
/// and merge `typographySizes` key-wise.
pub fn merge_persisted_settings(
    current: &Map<String, Value>,
    changes: &SanitizedUpdate,
) -> Map<String, Value> {
    let mut next = current.clone();
    for (key, value) in changes {
        match value {
            Some(v) => {
                next.insert(key.clone(), v.clone());
            }
            None => {
                next.remove(key.as_str());
            }
        }
    }

    // securityScopedBookmarks: prefer the update's array, else current's.
    let base_bookmarks: Option<Vec<String>> = changes
        .get("securityScopedBookmarks")
        .and_then(|v| v.as_ref())
        .and_then(Value::as_array)
        .map(|items| normalize_string_array(&Value::Array(items.clone())))
        .or_else(|| {
            current
                .get("securityScopedBookmarks")
                .and_then(Value::as_array)
                .map(|items| normalize_string_array(&Value::Array(items.clone())))
        });
    let bookmarks = base_bookmarks.unwrap_or_default();
    next.insert(
        "securityScopedBookmarks".into(),
        Value::Array(bookmarks.into_iter().map(Value::from).collect()),
    );

    // typographySizes: key-wise merge of current then changes.
    match changes.get("typographySizes").and_then(|v| v.as_ref()) {
        Some(incoming) => {
            let mut merged = Map::new();
            if let Some(existing) = current.get("typographySizes") {
                spread_into(&mut merged, existing);
            }
            spread_into(&mut merged, incoming);
            next.insert("typographySizes".into(), Value::Object(merged));
        }
        None => {
            if let Some(existing) = current.get("typographySizes") {
                next.insert("typographySizes".into(), existing.clone());
            } else {
                next.remove("typographySizes");
            }
        }
    }

    next
}

// ---------------------------------------------------------------------------
// formatSettingsResponse
// ---------------------------------------------------------------------------

/// `isAgentMemoryFeatureAvailable` — read per call, never baked in.
fn is_agent_memory_feature_available() -> bool {
    std::env::var("OMPCHAMBER_MEMORY_ENABLE")
        .map(|raw| {
            matches!(
                raw.trim().to_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

/// Port of `formatSettingsResponse(settings)`: sanitized settings minus the
/// managed tunnel token, plus server-computed fields.
pub fn format_settings_response(settings: &Map<String, Value>) -> Result<Value, AppError> {
    let mut sanitized = sanitize_settings_update(&Value::Object(settings.clone()))?;
    sanitized.remove("managedRemoteTunnelToken");

    let mut response = Map::new();
    for (key, value) in sanitized {
        if let Some(value) = value {
            response.insert(key, value);
        }
    }

    let token = settings
        .get("managedRemoteTunnelToken")
        .and_then(Value::as_str)
        .map(str::trim)
        .is_some_and(|t| !t.is_empty());
    response.insert("hasManagedRemoteTunnelToken".into(), Value::from(token));
    // Tells the client whether agent memory exists in this build at all, so
    // its settings row and panel tab can be absent rather than merely off.
    response.insert(
        "agentMemoryFeatureAvailable".into(),
        Value::from(is_agent_memory_feature_available()),
    );

    let pwa_app_name =
        normalize_pwa_app_name(settings.get("pwaAppName").unwrap_or(&Value::Null), "");
    if !pwa_app_name.is_empty() {
        response.insert("pwaAppName".into(), Value::from(pwa_app_name));
    }
    response.insert(
        "pwaOrientation".into(),
        Value::from(normalize_pwa_orientation(
            settings.get("pwaOrientation").unwrap_or(&Value::Null),
            "system",
        )),
    );
    response.insert(
        "mobileKeyboardMode".into(),
        Value::from(normalize_mobile_keyboard_mode(
            settings.get("mobileKeyboardMode").unwrap_or(&Value::Null),
            "native",
        )),
    );

    let bookmarks = normalize_string_array(
        settings
            .get("securityScopedBookmarks")
            .unwrap_or(&Value::Null),
    );
    response.insert(
        "securityScopedBookmarks".into(),
        Value::Array(bookmarks.into_iter().map(Value::from).collect()),
    );
    let pinned = normalize_string_array(settings.get("pinnedDirectories").unwrap_or(&Value::Null));
    response.insert(
        "pinnedDirectories".into(),
        Value::Array(pinned.into_iter().map(Value::from).collect()),
    );

    if let Some(typography) = sanitize_typography_sizes_partial(settings.get("typographySizes")) {
        response.insert("typographySizes".into(), Value::Object(typography));
    }

    if std::env::var("OMPCHAMBER_RUNTIME").as_deref() == Ok("desktop") {
        response.insert(
            "desktopLanAccessActive".into(),
            Value::from(
                std::env::var("OMPCHAMBER_DESKTOP_LAN_ACCESS_ACTIVE").as_deref() == Ok("true"),
            ),
        );
        response.insert(
            "desktopLanAccessBlockedReason".into(),
            if std::env::var("OMPCHAMBER_DESKTOP_LAN_ACCESS_BLOCKED_REASON").as_deref()
                == Ok("missing-password")
            {
                Value::from("missing-password")
            } else {
                Value::Null
            },
        );
    }

    response.insert(
        "showReasoningTraces".into(),
        Value::from(
            settings
                .get("showReasoningTraces")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        ),
    );
    response.insert(
        "collapsibleThinkingBlocks".into(),
        Value::from(
            settings
                .get("collapsibleThinkingBlocks")
                .and_then(Value::as_bool)
                .unwrap_or(true),
        ),
    );

    Ok(Value::Object(response))
}
