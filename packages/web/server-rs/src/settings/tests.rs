//! Unit tests for the settings module port. Mirrors the JS test suites
//! (`settings-helpers.test.js`, `settings-normalization-runtime.test.js`,
//! `settings-runtime.test.js`) plus route-level behavior via `oneshot`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::{Map, Value, json};
use tower::ServiceExt;

use super::helpers::{
    format_settings_response, merge_persisted_settings, sanitize_settings_update,
};
use super::normalization as norm;
use super::runtime::SettingsStore;
use super::{router, store_for_path};
use crate::config::{EngineConfig, ServerConfig};
use crate::context::RouterContext;
use crate::engine::EngineState;
use crate::hub::EventHub;

fn temp_dir_unique(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir =
        std::env::temp_dir().join(format!("oc-settings-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn store_for_temp(tag: &str) -> (Arc<SettingsStore>, PathBuf) {
    let dir = temp_dir_unique(tag);
    let store = store_for_path(&dir.join("settings.json"));
    (store, dir)
}

fn object(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap()
}

fn sanitized(payload: Value) -> super::helpers::SanitizedUpdate {
    sanitize_settings_update(&payload).unwrap()
}

// ---------------------------------------------------------------------------
// normalization.rs
// ---------------------------------------------------------------------------

#[test]
fn normalize_directory_path_strips_quotes_and_expands_home() {
    assert_eq!(norm::normalize_directory_path("  '/tmp/x'  "), "/tmp/x");
    // Quotes are stripped before ~ expansion, so a quoted tilde path expands.
    let home = norm::home_dir().unwrap();
    assert_eq!(
        Path::new(&norm::normalize_directory_path("\"~/proj\"")),
        home.join("proj")
    );
    assert_eq!(Path::new(&norm::normalize_directory_path("~")), home);
    assert_eq!(
        Path::new(&norm::normalize_directory_path("~/sub/dir")),
        home.join("sub/dir")
    );
    // A single quote character is never part of a real path but an unbalanced
    // one must survive untouched.
    assert_eq!(norm::normalize_directory_path("it's here"), "it's here");
}

#[test]
fn normalize_path_for_persistence_resolves_symlinks() {
    let dir = temp_dir_unique("norm-symlink");
    let real = dir.join("real");
    std::fs::create_dir_all(&real).unwrap();
    let link = dir.join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    let normalized = norm::normalize_path_for_persistence(
        &Value::from(link.to_string_lossy().into_owned()),
        true,
    );
    assert_eq!(
        normalized.as_str().unwrap(),
        std::fs::canonicalize(&real).unwrap().to_string_lossy()
    );
}

#[test]
fn normalize_path_for_persistence_falls_back_when_path_missing() {
    let normalized =
        norm::normalize_path_for_persistence(&Value::from("/definitely/not/here"), true);
    assert_eq!(normalized.as_str().unwrap(), "/definitely/not/here");
    // Non-strings pass through untouched.
    assert_eq!(
        norm::normalize_path_for_persistence(&Value::from(42), true),
        Value::from(42)
    );
}

#[test]
fn sanitize_projects_dedupes_and_validates() {
    let dir = temp_dir_unique("sanitize-projects");
    let project_path = dir.join("project");
    let other_path = dir.join("other");
    std::fs::create_dir_all(&project_path).unwrap();
    std::fs::create_dir_all(&other_path).unwrap();
    let path_str = project_path.to_string_lossy().into_owned();
    let other_str = other_path.to_string_lossy().into_owned();

    let input = json!([
        { "id": "path_a", "path": path_str, "label": " A ", "iconBackground": "#AABBCC",
          "defaultModel": "anthropic/claude", "defaultVariant": "high",
          "addedAt": 5, "lastOpenedAt": -3, "sidebarCollapsed": true, "iconImage": null },
        { "id": "path_a", "path": path_str },               // duplicate id
        { "id": "path_b", "path": path_str },               // duplicate path
        { "id": "", "path": path_str },                     // missing id
        { "id": "path_d", "path": other_str, "defaultModel": "no-slash", "defaultVariant": "high",
          "iconBackground": "red", "addedAt": "not-a-number",
          "iconImage": { "mime": "image/png", "updatedAt": 10, "source": "custom" } },
    ]);
    let projects = norm::sanitize_projects(Some(&input)).unwrap();
    assert_eq!(projects.len(), 2);

    let first = object(projects[0].clone());
    assert_eq!(first.get("id").unwrap(), "path_a");
    assert_eq!(first.get("label").unwrap(), "A");
    assert_eq!(first.get("iconBackground").unwrap(), "#aabbcc");
    assert_eq!(first.get("defaultModel").unwrap(), "anthropic/claude");
    assert_eq!(first.get("defaultVariant").unwrap(), "high");
    assert_eq!(first.get("addedAt").unwrap(), &Value::from(5));
    assert!(
        first.get("lastOpenedAt").is_none(),
        "negative timestamps are dropped"
    );
    assert_eq!(first.get("iconImage").unwrap(), &Value::Null);
    assert_eq!(first.get("sidebarCollapsed").unwrap(), &Value::from(true));

    let second = object(projects[1].clone());
    assert_eq!(second.get("id").unwrap(), "path_d");
    assert!(
        second.get("defaultModel").is_none(),
        "model refs without a provider are dropped"
    );
    assert!(
        second.get("defaultVariant").is_none(),
        "a variant is meaningless without its model"
    );
    assert!(
        second.get("iconBackground").is_none(),
        "non-hex backgrounds are dropped"
    );
    assert!(
        second.get("addedAt").is_none(),
        "non-numeric timestamps are dropped"
    );
    assert_eq!(
        second.get("iconImage").unwrap(),
        &json!({ "mime": "image/png", "updatedAt": 10, "source": "custom" })
    );
}

#[test]
fn normalize_string_array_dedupes_and_keeps_order() {
    let value = json!(["b", "a", "b", "", 3, "a"]);
    assert_eq!(norm::normalize_string_array(&value), vec!["b", "a"]);
    assert!(norm::normalize_string_array(&json!("not-an-array")).is_empty());
}

#[test]
fn managed_tunnel_hostname_normalization() {
    let cases: Vec<(Value, Option<&str>)> = vec![
        (json!("Cloudflare.Example/x"), Some("cloudflare.example")),
        (
            json!("https://Host.Example:8080/path"),
            Some("host.example"),
        ),
        (json!("  "), None),
        (json!(123), None),
        (json!("not a url !!"), None),
    ];
    for (input, expected) in cases {
        assert_eq!(
            norm::normalize_managed_remote_tunnel_hostname(&input),
            expected.map(String::from)
        );
    }
}

#[test]
fn managed_tunnel_presets_dedupe_by_id_and_hostname() {
    let input = json!([
        { "id": " a ", "name": " A ", "hostname": "A.Example" },
        { "id": "a", "name": "dup", "hostname": "other.example" },
        { "id": "b", "name": "B", "hostname": "a.example" },
        { "id": "", "name": "C", "hostname": "c.example" },
        { "id": "d", "name": "", "hostname": "d.example" },
    ]);
    let presets = norm::normalize_managed_remote_tunnel_presets(&input).unwrap();
    assert_eq!(presets.len(), 1);
    assert_eq!(
        presets[0],
        json!({ "id": "a", "name": "A", "hostname": "a.example" })
    );
    assert!(norm::normalize_managed_remote_tunnel_presets(&json!("x")).is_none());
}

#[test]
fn managed_tunnel_preset_tokens_filter_empty() {
    let tokens = norm::normalize_managed_remote_tunnel_preset_tokens(
        &json!({ " a ": " t ", "": "x", "b": "" }),
    )
    .unwrap();
    assert_eq!(tokens.get("a").unwrap(), "t");
    assert_eq!(tokens.len(), 1);
    assert!(norm::normalize_managed_remote_tunnel_preset_tokens(&json!({ "a": "" })).is_none());
    assert!(norm::normalize_managed_remote_tunnel_preset_tokens(&json!([1])).is_none());
}

#[test]
fn typography_sizes_partial_keeps_known_non_empty_keys() {
    let typography = norm::sanitize_typography_sizes_partial(Some(
        &json!({ "markdown": "lg", "code": "", "other": "x" }),
    ))
    .unwrap();
    assert_eq!(typography.len(), 1);
    assert_eq!(typography.get("markdown").unwrap(), "lg");
    assert!(norm::sanitize_typography_sizes_partial(Some(&json!({}))).is_none());
    assert!(norm::sanitize_typography_sizes_partial(None).is_none());
}

#[test]
fn model_refs_limit_and_dedupe() {
    let input = json!([
        { "providerID": " anthropic ", "modelID": " opus " },
        { "providerID": "anthropic", "modelID": "opus" },
        { "providerID": "openai", "modelID": "gpt" },
        "junk",
        { "providerID": "", "modelID": "x" },
    ]);
    let refs = norm::sanitize_model_refs(&input, 2).unwrap();
    assert_eq!(
        refs,
        vec![
            json!({ "providerID": "anthropic", "modelID": "opus" }),
            json!({ "providerID": "openai", "modelID": "gpt" }),
        ]
    );
    assert!(norm::sanitize_model_refs(&json!("x"), 5).is_none());
}

#[test]
fn skill_catalogs_keep_required_fields() {
    let input = json!([
        { "id": " core ", "label": "Core", "source": "git", "subpath": "skills/core", "gitIdentityId": "gh" },
        { "id": "core", "label": "dup", "source": "git" },
        { "id": "x", "label": "", "source": "git" },
    ]);
    let catalogs = norm::sanitize_skill_catalogs(&input).unwrap();
    assert_eq!(catalogs.len(), 1);
    assert_eq!(
        catalogs[0],
        json!({ "id": "core", "label": "Core", "source": "git", "subpath": "skills/core", "gitIdentityId": "gh" })
    );
}

#[test]
fn tunnel_ttl_clamps_use_production_constants() {
    assert_eq!(
        norm::normalize_tunnel_bootstrap_ttl_ms(&Value::Null),
        Value::Null
    );
    assert_eq!(
        norm::normalize_tunnel_bootstrap_ttl_ms(&json!(1_000)),
        json!(60_000)
    );
    assert_eq!(
        norm::normalize_tunnel_bootstrap_ttl_ms(&json!(1e12)),
        json!(86_400_000)
    );
    assert_eq!(
        norm::normalize_tunnel_bootstrap_ttl_ms(&json!("x")),
        json!(1_800_000)
    );
    assert_eq!(
        norm::normalize_tunnel_session_ttl_ms(&json!("x")),
        json!(28_800_000)
    );
    assert_eq!(
        norm::normalize_tunnel_session_ttl_ms(&json!(1_000)),
        json!(300_000)
    );
    assert_eq!(
        norm::normalize_tunnel_session_ttl_ms(&json!(1e12)),
        json!(2_592_000_000i64)
    );
}

#[test]
fn tunnel_provider_and_mode_default_sanely() {
    assert_eq!(norm::normalize_tunnel_provider(&json!(null)), "cloudflare");
    assert_eq!(norm::normalize_tunnel_provider(&json!(" NGROK ")), "ngrok");
    assert_eq!(
        norm::normalize_tunnel_provider(&json!("bogus")),
        "cloudflare"
    );
    assert_eq!(norm::normalize_tunnel_mode(&json!(null)), "quick");
    assert_eq!(
        norm::normalize_tunnel_mode(&json!(" Managed-Remote ")),
        "managed-remote"
    );
    assert_eq!(norm::normalize_tunnel_mode(&json!("nope")), "quick");
}

#[test]
fn optional_tunnel_path_must_stay_in_home() {
    let inside = norm::normalize_optional_path("~/tunnel/config.yml").unwrap();
    let home = norm::home_dir().unwrap();
    assert_eq!(Path::new(&inside), home.join("tunnel/config.yml"));
    let err = norm::normalize_optional_path("/etc/passwd").unwrap_err();
    assert!(err.to_string().contains("within the home directory"));
}

#[test]
fn project_ids_are_path_base64url() {
    use base64::Engine as _;
    let expected = format!(
        "path_{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"/a/b")
    );
    assert_eq!(norm::create_project_id_from_path("/a/b/"), expected);
    assert_eq!(norm::create_project_id_from_path("\\a\\b"), expected);
    assert_eq!(norm::create_project_id_from_path("  "), "");
}

#[test]
fn sha1_matches_known_vectors() {
    assert_eq!(
        norm::sha1_hex("abc"),
        "a9993e364706816aba3e25717850c26c9cd0d89d"
    );
    assert_eq!(
        norm::sha1_hex(""),
        "da39a3ee5e6b4b0d3255bfef95601890afd80709"
    );
}

#[test]
fn path_resolve_lexically_normalizes() {
    let cwd = std::env::current_dir().unwrap();
    assert_eq!(norm::path_resolve("x/y"), cwd.join("x/y"));
    assert_eq!(norm::path_resolve("/a/b/../c"), PathBuf::from("/a/c"));
    assert_eq!(norm::path_resolve("/a/./b"), PathBuf::from("/a/b"));
    assert_eq!(norm::path_relative("/a/b", "/a/b/c/d"), "c/d");
    assert_eq!(norm::path_relative("/a/b/c", "/a/x"), "../../x");
    assert_eq!(norm::path_relative("/a/b", "/a/b"), "");
}

// ---------------------------------------------------------------------------
// helpers.rs — sanitizeSettingsUpdate
// ---------------------------------------------------------------------------

#[test]
fn sanitize_accepts_only_booleans() {
    assert_eq!(
        sanitized(json!({ "collapsibleThinkingBlocks": true })).get("collapsibleThinkingBlocks"),
        Some(&Some(Value::from(true)))
    );
    for bad in [json!("true"), json!(1), json!(null)] {
        let update = sanitized(json!({ "collapsibleThinkingBlocks": bad }));
        assert!(
            !update.contains_key("collapsibleThinkingBlocks"),
            "rejected {bad}"
        );
    }
}

#[test]
fn sanitize_clamps_and_rounds_sizes() {
    let update = sanitized(
        json!({ "editorFontSize": 20.6, "fontSize": 999, "cornerRadius": -5, "messageLimit": 5 }),
    );
    assert_eq!(update.get("editorFontSize"), Some(&Some(json!(21))));
    assert_eq!(update.get("fontSize"), Some(&Some(json!(200))));
    assert_eq!(update.get("cornerRadius"), Some(&Some(json!(0))));
    assert_eq!(update.get("messageLimit"), Some(&Some(json!(10))));
}

#[test]
fn sanitize_terminal_shell_and_login_shells() {
    let update = sanitized(
        json!({ "terminalShell": " ZSH ", "terminalLoginShells": ["Fish ", "bogus", "FISH"] }),
    );
    assert_eq!(update.get("terminalShell"), Some(&Some(json!("zsh"))));
    assert_eq!(
        update.get("terminalLoginShells"),
        Some(&Some(json!(["fish"])))
    );
}

#[test]
fn sanitize_rejects_invalid_enum_values() {
    assert_eq!(
        sanitized(json!({ "sidebarProjectDisplayMode": "single" }))
            .get("sidebarProjectDisplayMode"),
        Some(&Some(json!("single")))
    );
    assert!(
        !sanitized(json!({ "messageStreamTransport": "websocket" }))
            .contains_key("messageStreamTransport")
    );
    assert!(
        !sanitized(json!({ "mobileKeyboardMode": "fixed-layout" }))
            .contains_key("mobileKeyboardMode")
    );
    assert!(
        !sanitized(json!({ "sessionRetentionAction": "remove" }))
            .contains_key("sessionRetentionAction")
    );
    assert!(
        !sanitized(json!({ "sidebarProjectDisplayMode": "many" }))
            .contains_key("sidebarProjectDisplayMode")
    );
    assert_eq!(
        sanitized(json!({ "desktopWindowControlsPosition": "auto" }))
            .get("desktopWindowControlsPosition"),
        Some(&Some(json!("right")))
    );
}

#[test]
fn sanitize_permission_auto_accept_policy() {
    let update = sanitized(json!({
        "permissionAutoAccept": { "sessions": { "root": true, "child": false, "invalid": "true" } }
    }));
    assert_eq!(
        update.get("permissionAutoAccept"),
        Some(&Some(
            json!({ "sessions": { "root": true, "child": false }, "revision": 0 })
        ))
    );
    let update = sanitized(json!({ "permissionAutoAccept": { "revision": 7 } }));
    assert_eq!(
        update.get("permissionAutoAccept"),
        Some(&Some(json!({ "sessions": {}, "revision": 7 })))
    );
}

#[test]
fn sanitize_trims_password_and_shortcut_overrides() {
    let update = sanitized(json!({ "desktopUiPassword": " secret " }));
    assert_eq!(
        update.get("desktopUiPassword"),
        Some(&Some(json!("secret")))
    );
    let update = sanitized(json!({ "desktopUiPassword": "" }));
    assert_eq!(update.get("desktopUiPassword"), Some(&Some(json!(""))));

    let update = sanitized(json!({
        "shortcutOverrides": {
            "open_settings": "mod+comma",
            "new_chat": "__unassigned__",
            "invalid": 123,
            "empty": ""
        }
    }));
    assert_eq!(
        update.get("shortcutOverrides"),
        Some(&Some(
            json!({ "open_settings": "mod+comma", "new_chat": "__unassigned__" })
        ))
    );
    // An explicit empty map survives (resetting all shortcuts).
    assert_eq!(
        sanitized(json!({ "shortcutOverrides": {} })).get("shortcutOverrides"),
        Some(&Some(json!({})))
    );
}

#[test]
fn sanitize_model_lists_roundtrip_and_reject_garbage() {
    let refs = json!([{ "providerID": "anthropic", "modelID": "claude-opus-4" }]);
    assert_eq!(
        sanitized(json!({ "hiddenModels": refs })).get("hiddenModels"),
        Some(&Some(refs.clone()))
    );
    assert_eq!(
        sanitized(json!({ "hiddenModels": [] })).get("hiddenModels"),
        Some(&Some(json!([])))
    );
    for bad in [json!("not-an-array"), json!(null), json!(123)] {
        assert!(!sanitized(json!({ "hiddenModels": bad })).contains_key("hiddenFields_or_marker"));
        assert!(
            !sanitized(json!({ "hiddenModels": bad })).contains_key("hiddenModels"),
            "rejected {bad}"
        );
    }
    let update = sanitized(json!({
        "hiddenModels": [
            { "providerID": "anthropic" },
            { "modelID": "gpt-5" },
            "not-an-object",
            null,
            { "providerID": "  ", "modelID": "x" },
            { "providerID": "openai", "modelID": "" },
        ]
    }));
    assert_eq!(update.get("hiddenModels"), Some(&Some(json!([]))));

    assert_eq!(
        sanitized(json!({ "collapsedModelProviders": ["anthropic", "openai", "anthropic"] }))
            .get("collapsedModelProviders"),
        Some(&Some(json!(["anthropic", "openai"])))
    );
    assert!(
        !sanitized(json!({ "collapsedModelProviders": "anthropic" }))
            .contains_key("collapsedModelProviders")
    );
    assert!(!sanitized(json!({ "recentAgents": { "build": 1 } })).contains_key("recentAgents"));
}

#[test]
fn sanitize_recent_efforts() {
    let input =
        json!({ "anthropic/claude-opus-4": ["high", "high", "default"], "openai/gpt-5": ["low"] });
    assert_eq!(
        sanitized(json!({ "recentEfforts": input })).get("recentEfforts"),
        Some(&Some(
            json!({ "anthropic/claude-opus-4": ["high", "default"], "openai/gpt-5": ["low"] })
        ))
    );
    for bad in [
        json!("not-an-object"),
        json!([]),
        json!(null),
        json!({ "k": "high" }),
        json!({ "": ["high"] }),
        json!({ "k": [] }),
        json!({ "k": [123, ""] }),
    ] {
        assert!(
            !sanitized(json!({ "recentEfforts": bad })).contains_key("recentEfforts"),
            "rejected {bad}"
        );
    }
}

#[test]
fn sanitize_drops_unknown_and_retired_keys() {
    let update = sanitized(json!({
        "optimizeSystemPrompt": true,
        "showOpenCodeUpdateNotifications": true,
        "openCodeUpdateToastDismissedVersion": "1.16.0",
        "totallyUnknownKey": "x",
    }));
    assert!(update.is_empty());
}

#[test]
fn sanitize_clears_optional_fields_on_empty_strings() {
    let update = sanitized(json!({ "defaultModel": "  " }));
    // `None` models the JS `undefined` assignment: the key clears persisted state.
    assert_eq!(update.get("defaultModel"), Some(&None));
    let update = sanitized(json!({ "managedRemoteTunnelSelectedPresetId": "  " }));
    assert_eq!(
        update.get("managedRemoteTunnelSelectedPresetId"),
        Some(&None)
    );
}

#[test]
fn sanitize_tunnel_fields() {
    let update = sanitized(json!({
        "tunnelBootstrapTtlMs": null,
        "tunnelSessionTtlMs": 1_000,
        "tunnelProvider": "bogus",
        "tunnelMode": "Managed-Remote",
        "managedRemoteTunnelHostname": "https://Tunnel.Example/x",
        "managedRemoteTunnelToken": " tok ",
        "managedRemoteTunnelPresets": [],
        "managedRemoteTunnelPresetTokens": { "a": "t" },
    }));
    assert_eq!(update.get("tunnelBootstrapTtlMs"), Some(&Some(Value::Null)));
    assert_eq!(
        update.get("tunnelSessionTtlMs"),
        Some(&Some(json!(300_000)))
    );
    assert_eq!(
        update.get("tunnelProvider"),
        Some(&Some(json!("cloudflare")))
    );
    assert_eq!(
        update.get("tunnelMode"),
        Some(&Some(json!("managed-remote")))
    );
    assert_eq!(
        update.get("managedRemoteTunnelHostname"),
        Some(&Some(json!("tunnel.example")))
    );
    assert_eq!(
        update.get("managedRemoteTunnelToken"),
        Some(&Some(json!("tok")))
    );
    assert_eq!(
        update.get("managedRemoteTunnelPresets"),
        Some(&Some(json!([])))
    );
    assert_eq!(
        update.get("managedRemoteTunnelPresetTokens"),
        Some(&Some(json!({ "a": "t" })))
    );
    // A null token clears; a config path outside $HOME fails the update.
    assert_eq!(
        sanitized(json!({ "managedRemoteTunnelToken": null })).get("managedRemoteTunnelToken"),
        Some(&Some(Value::Null))
    );
    assert_eq!(
        sanitized(json!({ "managedLocalTunnelConfigPath": null }))
            .get("managedLocalTunnelConfigPath"),
        Some(&Some(Value::Null))
    );
    let err = sanitize_settings_update(
        &json!({ "managedLocalTunnelConfigPath": "/etc/cloudflared.yml" }),
    )
    .unwrap_err();
    assert!(err.to_string().contains("within the home directory"));
}

#[test]
fn sanitize_theme_fields() {
    let update = sanitized(json!({
        "themeId": "default",
        "themeVariant": "dark",
        "splashBgLight": "  #fff  ",
        "splashBgDark": "   ",
    }));
    assert_eq!(update.get("themeId"), Some(&Some(json!("default"))));
    assert_eq!(update.get("themeVariant"), Some(&Some(json!("dark"))));
    assert_eq!(update.get("splashBgLight"), Some(&Some(json!("#fff"))));
    assert!(!update.contains_key("splashBgDark"));
    assert!(!sanitized(json!({ "themeVariant": "blue" })).contains_key("themeVariant"));
}

#[test]
fn sanitize_stt_and_followup_fields() {
    let update = sanitized(json!({
        "sttProvider": "server",
        "sttServerUrl": " http://x ",
        "followUpBehavior": "immediate",
    }));
    assert_eq!(
        update.get("sttProvider"),
        Some(&Some(json!("openai-compatible")))
    );
    assert_eq!(update.get("sttServerUrl"), Some(&Some(json!("http://x"))));
    assert_eq!(update.get("followUpBehavior"), Some(&Some(json!("steer"))));
    assert_eq!(
        sanitized(json!({ "queueModeEnabled": false })).get("followUpBehavior"),
        Some(&Some(json!("steer")))
    );
    assert_eq!(
        sanitized(json!({ "queueModeEnabled": true })).get("followUpBehavior"),
        Some(&Some(json!("queue")))
    );
    let update = sanitized(json!({ "pwaAppName": "  My   App  ", "pwaOrientation": "bogus" }));
    assert_eq!(update.get("pwaAppName"), Some(&Some(json!("My App"))));
    assert_eq!(update.get("pwaOrientation"), Some(&None));
}

// ---------------------------------------------------------------------------
// helpers.rs — mergePersistedSettings / formatSettingsResponse
// ---------------------------------------------------------------------------

#[test]
fn merge_persists_clears_and_dedupes_bookmarks() {
    let current = object(json!({
        "themeId": "keep-me",
        "clearedKey": "old",
        "securityScopedBookmarks": ["a", "a", ""],
        "typographySizes": { "markdown": "lg" },
    }));
    let mut changes = super::helpers::SanitizedUpdate::new();
    changes.insert("clearedKey".into(), None);
    changes.insert("newKey".into(), Some(json!(2)));
    changes.insert("typographySizes".into(), Some(json!({ "code": "sm" })));

    let merged = merge_persisted_settings(&current, &changes);
    assert_eq!(merged.get("themeId"), Some(&json!("keep-me")));
    assert!(merged.get("clearedKey").is_none());
    assert_eq!(merged.get("newKey"), Some(&json!(2)));
    assert_eq!(merged.get("securityScopedBookmarks"), Some(&json!(["a"])));
    assert_eq!(
        merged.get("typographySizes"),
        Some(&json!({ "markdown": "lg", "code": "sm" }))
    );

    // Merge always materializes securityScopedBookmarks (possibly empty).
    let empty = merge_persisted_settings(&Map::new(), &super::helpers::SanitizedUpdate::new());
    assert_eq!(empty.get("securityScopedBookmarks"), Some(&json!([])));
    assert!(empty.get("typographySizes").is_none());
}

#[test]
fn format_response_defaults_and_hides_tunnel_token() {
    let settings = object(json!({
        "managedRemoteTunnelToken": "tok",
        "collapsibleThinkingBlocks": false,
        "desktopUiPassword": "secret",
    }));
    let response = format_settings_response(&settings).unwrap();
    let response = response.as_object().unwrap();
    assert!(
        response.get("managedRemoteTunnelToken").is_none(),
        "the token never reaches the wire"
    );
    assert_eq!(
        response.get("hasManagedRemoteTunnelToken"),
        Some(&json!(true))
    );
    assert_eq!(
        response.get("collapsibleThinkingBlocks"),
        Some(&json!(false))
    );
    assert_eq!(response.get("desktopUiPassword"), Some(&json!("secret")));

    let response = format_settings_response(&Map::new()).unwrap();
    let response = response.as_object().unwrap();
    assert_eq!(
        response.get("collapsibleThinkingBlocks"),
        Some(&json!(true))
    );
    assert_eq!(response.get("showReasoningTraces"), Some(&json!(false)));
    assert_eq!(
        response.get("hasManagedRemoteTunnelToken"),
        Some(&json!(false))
    );
    assert_eq!(
        response.get("agentMemoryFeatureAvailable"),
        Some(&json!(false))
    );
    if std::env::var("OMPCHAMBER_RUNTIME").as_deref() == Ok("desktop") {
        assert!(response.contains_key("desktopLanAccessActive"));
    } else {
        assert!(
            response.get("desktopLanAccessActive").is_none(),
            "desktop fields need the desktop env"
        );
    }
    assert_eq!(response.get("mobileKeyboardMode"), Some(&json!("native")));
    assert_eq!(response.get("securityScopedBookmarks"), Some(&json!([])));
    assert_eq!(response.get("pinnedDirectories"), Some(&json!([])));
    assert!(
        response.get("typographySizes").is_none(),
        "undefined typography is dropped like JSON.stringify"
    );
}

// ---------------------------------------------------------------------------
// runtime.rs — SettingsStore
// ---------------------------------------------------------------------------

#[tokio::test]
async fn missing_file_yields_migrated_defaults_and_persists_them() {
    let (store, _dir) = store_for_temp("missing");

    let settings = store.read_migrated().await.unwrap();
    assert_eq!(settings.get("lightThemeId"), Some(&json!("flexoki-light")));
    assert_eq!(settings.get("darkThemeId"), Some(&json!("flexoki-dark")));
    for key in [
        "notifyOnSubtasks",
        "notifyOnCompletion",
        "notifyOnError",
        "notifyOnQuestion",
    ] {
        assert_eq!(
            settings.get(key),
            Some(&json!(true)),
            "{key} defaults to true"
        );
    }
    let templates = settings
        .get("notificationTemplates")
        .unwrap()
        .as_object()
        .unwrap();
    assert_eq!(templates.len(), 4);
    assert_eq!(
        templates.get("completion").unwrap().get("title").unwrap(),
        "{agent_name} is ready"
    );
    assert_eq!(settings.get("securityScopedBookmarks"), Some(&json!([])));

    // The migration was persisted to disk.
    let raw = tokio::fs::read_to_string(store.settings_path())
        .await
        .unwrap();
    let persisted: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(persisted.get("lightThemeId"), Some(&json!("flexoki-light")));
    // A second migrated read is stable (no further changes).
    let again = store.read_migrated().await.unwrap();
    assert_eq!(again, settings);
}

#[tokio::test]
async fn malformed_json_is_an_error_for_strict_reads_only() {
    let (store, _dir) = store_for_temp("malformed");
    tokio::fs::write(store.settings_path(), "{oops")
        .await
        .unwrap();

    let strict = store.read_strict().await;
    assert!(
        strict.is_err(),
        "strict read must never treat corruption as a first run"
    );
    assert!(strict.unwrap_err().to_string().contains("parse"));

    // The lenient reader (used by GET) mirrors the JS: warn and start from {}.
    let settings = store.read_migrated().await.unwrap();
    assert_eq!(settings.get("lightThemeId"), Some(&json!("flexoki-light")));
}

#[tokio::test]
async fn non_object_payload_is_an_error_for_strict_reads() {
    let (store, _dir) = store_for_temp("nonobject");
    tokio::fs::write(store.settings_path(), "[1,2]")
        .await
        .unwrap();
    let err = store.read_strict().await.unwrap_err();
    assert!(err.to_string().contains("malformed"));
}

#[tokio::test]
async fn persist_sanitizes_and_roundtrips() {
    let (store, _dir) = store_for_temp("persist");

    let response = store
        .persist(&json!({
            "fontSize": 999,
            "themeId": "dark-x",
            "desktopUiPassword": " p ",
            "unknownKey": "dropped",
            "messageStreamTransport": "websocket",
        }))
        .await
        .unwrap();
    let response = response.as_object().unwrap();
    assert_eq!(response.get("fontSize"), Some(&json!(200)));
    assert_eq!(response.get("desktopUiPassword"), Some(&json!("p")));
    assert!(!response.contains_key("unknownKey"));
    assert!(!response.contains_key("messageStreamTransport"));

    let raw = store.read_raw().await;
    assert_eq!(raw.get("fontSize"), Some(&json!(200)));
    assert_eq!(raw.get("themeId"), Some(&json!("dark-x")));
    assert!(raw.get("unknownKey").is_none());

    // A second update keeps the first update's fields (merge, not replace).
    store.persist(&json!({ "themeId": "other" })).await.unwrap();
    let raw = store.read_raw().await;
    assert_eq!(raw.get("themeId"), Some(&json!("other")));
    assert_eq!(raw.get("fontSize"), Some(&json!(200)));
}

#[tokio::test]
async fn persist_clears_default_model_with_empty_string() {
    let (store, _dir) = store_for_temp("clear");
    store
        .persist(&json!({ "defaultModel": "anthropic/claude" }))
        .await
        .unwrap();
    assert_eq!(
        store.read_raw().await.get("defaultModel"),
        Some(&json!("anthropic/claude"))
    );
    store.persist(&json!({ "defaultModel": "" })).await.unwrap();
    assert!(store.read_raw().await.get("defaultModel").is_none());
}

#[tokio::test]
async fn write_is_atomic_with_restrictive_permissions() {
    let (store, dir) = store_for_temp("atomic");
    store
        .write_raw(&object(json!({ "theme": "dark" })))
        .await
        .unwrap();

    let body = tokio::fs::read_to_string(store.settings_path())
        .await
        .unwrap();
    assert_eq!(
        body,
        serde_json::to_string_pretty(&json!({ "theme": "dark" })).unwrap()
    );

    // No temp files are left behind; siblings are untouched.
    let files: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(files, vec!["settings.json".to_string()]);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let file_mode = std::fs::metadata(store.settings_path())
            .unwrap()
            .permissions()
            .mode();
        let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(file_mode & 0o777, 0o600);
        assert_eq!(dir_mode & 0o777, 0o700);
    }
}

#[tokio::test]
async fn orphaned_tmp_files_are_cleaned_during_migrated_read() {
    let (store, dir) = store_for_temp("orphans");
    tokio::fs::write(dir.join("settings.json.tmp-1234-11111-abc"), "x")
        .await
        .unwrap();
    tokio::fs::write(dir.join("settings.json.tmp-5678-22222-def"), "x")
        .await
        .unwrap();
    tokio::fs::write(dir.join("other-file.json"), "keep")
        .await
        .unwrap();
    tokio::fs::write(store.settings_path(), r#"{"theme":"light"}"#)
        .await
        .unwrap();

    store.read_migrated().await.unwrap();

    let files: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(files.contains(&"settings.json".to_string()));
    assert!(files.contains(&"other-file.json".to_string()));
    assert!(!files.iter().any(|f| f.starts_with("settings.json.tmp-")));
}

#[tokio::test]
async fn unknown_disk_fields_survive_but_retired_keys_are_removed() {
    let (store, _dir) = store_for_temp("unknown");
    tokio::fs::write(
        store.settings_path(),
        serde_json::to_string(&json!({
            "customField": { "a": 1 },
            "fontSize": 14,
            "approvedDirectories": ["/x"],
            "showOpenCodeUpdateNotifications": true,
            "openCodeUpdateToastDismissedVersion": "1.18.8",
            "namedTunnelToken": "abc",
            "namedTunnelHostname": "Tunnel.Example",
        }))
        .unwrap(),
    )
    .await
    .unwrap();

    let settings = store.read_migrated().await.unwrap();
    assert_eq!(
        settings.get("customField"),
        Some(&json!({ "a": 1 })),
        "unknown fields survive migrations"
    );
    assert_eq!(settings.get("fontSize"), Some(&json!(14)));
    assert!(settings.get("approvedDirectories").is_none());
    assert!(settings.get("showOpenCodeUpdateNotifications").is_none());
    assert!(
        settings
            .get("openCodeUpdateToastDismissedVersion")
            .is_none()
    );
    assert!(settings.get("namedTunnelToken").is_none());
    assert!(settings.get("namedTunnelHostname").is_none());
    assert_eq!(
        settings.get("managedRemoteTunnelToken"),
        Some(&json!("abc"))
    );
    assert_eq!(
        settings.get("managedRemoteTunnelHostname"),
        Some(&json!("tunnel.example"))
    );

    // And they survive a subsequent persist.
    store.persist(&json!({ "themeId": "x" })).await.unwrap();
    assert_eq!(
        store.read_raw().await.get("customField"),
        Some(&json!({ "a": 1 }))
    );
}

#[tokio::test]
async fn deterministic_project_ids_remap_plan_files() {
    let dir = temp_dir_unique("det-ids");
    let project_path = dir.join("project");
    std::fs::create_dir_all(&project_path).unwrap();
    let canonical_project = std::fs::canonicalize(&project_path).unwrap();
    let new_id = norm::create_project_id_from_path(&canonical_project.to_string_lossy());

    let store = store_for_path(&dir.join("settings.json"));
    let projects_root = dir.join("projects");
    let old_id = "legacy-project-id";
    let old_storage = projects_root.join(old_id);
    let new_storage = projects_root.join(&new_id);
    let sibling_storage = projects_root.join(format!("{old_id}-sibling"));
    std::fs::create_dir_all(&old_storage).unwrap();
    std::fs::create_dir_all(&sibling_storage).unwrap();
    tokio::fs::write(
        store.settings_path(),
        serde_json::to_string(&json!({
            "projects": [{
                "id": old_id,
                "path": canonical_project.to_string_lossy(),
                "addedAt": 1,
                "lastOpenedAt": 1,
            }],
            "activeProjectId": old_id,
        }))
        .unwrap(),
    )
    .await
    .unwrap();
    tokio::fs::write(
        projects_root.join(format!("{old_id}.json")),
        serde_json::to_string(&json!({
            "projectPlanFiles": [
                { "id": "inside", "path": old_storage.join("plans/inside.md") },
                { "id": "sibling", "path": sibling_storage.join("plans/outside.md") },
            ],
        }))
        .unwrap(),
    )
    .await
    .unwrap();

    let settings = store.read_migrated().await.unwrap();

    let migrated: Value = serde_json::from_str(
        &tokio::fs::read_to_string(projects_root.join(format!("{new_id}.json")))
            .await
            .unwrap(),
    )
    .unwrap();
    let plan_files = migrated
        .get("projectPlanFiles")
        .unwrap()
        .as_array()
        .unwrap();
    assert_eq!(
        plan_files[0].get("path").unwrap(),
        &Value::from(
            new_storage
                .join("plans/inside.md")
                .to_string_lossy()
                .into_owned()
        ),
        "paths inside the migrated storage dir are remapped"
    );
    assert_eq!(
        plan_files[1].get("path").unwrap(),
        &Value::from(
            sibling_storage
                .join("plans/outside.md")
                .to_string_lossy()
                .into_owned()
        ),
        "sibling directories are left alone"
    );
    assert!(
        !projects_root.join(format!("{old_id}.json")).exists(),
        "legacy config removed"
    );
    assert!(!old_storage.exists(), "legacy storage dir moved");

    let projects = settings.get("projects").unwrap().as_array().unwrap();
    assert_eq!(projects[0].get("id").unwrap(), &Value::from(new_id.clone()));
    assert_eq!(
        settings.get("activeProjectId"),
        Some(&Value::from(new_id.clone()))
    );
}

#[tokio::test]
async fn collapsed_projects_migrate_to_sidebar_flags() {
    let (store, _dir) = store_for_temp("collapsed");
    let project_dir = store.settings_path().parent().unwrap().join("p");
    std::fs::create_dir_all(&project_dir).unwrap();
    let canonical = std::fs::canonicalize(&project_dir).unwrap();
    let id = norm::create_project_id_from_path(&canonical.to_string_lossy());
    tokio::fs::write(
        store.settings_path(),
        serde_json::to_string(&json!({
            "projects": [{ "id": id, "path": canonical.to_string_lossy() }],
            "collapsedProjects": [id],
        }))
        .unwrap(),
    )
    .await
    .unwrap();

    let settings = store.read_migrated().await.unwrap();
    assert!(settings.get("collapsedProjects").is_none());
    let projects = settings.get("projects").unwrap().as_array().unwrap();
    assert_eq!(projects[0].get("sidebarCollapsed"), Some(&json!(true)));
}

#[tokio::test]
async fn legacy_last_directory_seeds_a_project() {
    let (store, dir) = store_for_temp("lastdir");
    let workspace = dir.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let canonical = std::fs::canonicalize(&workspace).unwrap();
    tokio::fs::write(
        store.settings_path(),
        serde_json::to_string(&json!({ "lastDirectory": canonical.to_string_lossy() })).unwrap(),
    )
    .await
    .unwrap();

    let settings = store.read_migrated().await.unwrap();
    let projects = settings.get("projects").unwrap().as_array().unwrap();
    assert_eq!(projects.len(), 1);
    assert_eq!(
        projects[0].get("path").unwrap(),
        &Value::from(canonical.to_string_lossy().into_owned())
    );
    assert_eq!(
        settings.get("activeProjectId"),
        Some(&Value::from(norm::create_project_id_from_path(
            &canonical.to_string_lossy()
        )))
    );
}

#[tokio::test]
async fn persist_drops_projects_whose_directory_is_gone() {
    let (store, _dir) = store_for_temp("validate");
    let live_dir = store.settings_path().parent().unwrap().join("live");
    std::fs::create_dir_all(&live_dir).unwrap();
    let canonical_live = std::fs::canonicalize(&live_dir).unwrap();
    let live_id = norm::create_project_id_from_path(&canonical_live.to_string_lossy());
    let dead_id = norm::create_project_id_from_path("/definitely/not/a/real/dir");

    let response = store
        .persist(&json!({
            "projects": [
                { "id": dead_id, "path": "/definitely/not/a/real/dir" },
                { "id": live_id, "path": canonical_live.to_string_lossy() },
            ],
        }))
        .await
        .unwrap();

    let projects = response.get("projects").unwrap().as_array().unwrap();
    assert_eq!(projects.len(), 1);
    assert_eq!(
        projects[0].get("id").unwrap(),
        &Value::from(live_id.clone())
    );
    assert_eq!(
        response.get("activeProjectId"),
        Some(&Value::from(live_id.clone()))
    );

    let raw = store.read_raw().await;
    assert_eq!(raw.get("projects").unwrap().as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn typed_view_roundtrips_unknown_fields() {
    let (store, _dir) = store_for_temp("typed");
    tokio::fs::write(
        store.settings_path(),
        serde_json::to_string(&json!({
            "fontSize": 14,
            "themeId": "default",
            "customField": { "a": 1 },
            "projects": [{ "id": "path_x", "path": "/tmp/x", "futureKey": true }],
        }))
        .unwrap(),
    )
    .await
    .unwrap();

    let typed = store.read_typed().await.unwrap();
    assert_eq!(
        typed.font_size.as_ref().and_then(|n| n.as_f64()),
        Some(14.0)
    );
    assert_eq!(typed.theme_id.as_deref(), Some("default"));
    assert_eq!(typed.extra.get("customField"), Some(&json!({ "a": 1 })));
    assert_eq!(typed.projects.as_ref().unwrap().len(), 1);
    assert_eq!(
        typed.projects.as_ref().unwrap()[0].extra.get("futureKey"),
        Some(&json!(true))
    );

    let serialized = serde_json::to_value(&typed).unwrap();
    assert_eq!(serialized.get("customField"), Some(&json!({ "a": 1 })));
    assert_eq!(serialized.get("fontSize"), Some(&json!(14)));
    assert_eq!(serialized.get("themeId"), Some(&json!("default")));
    assert_eq!(
        serialized
            .get("projects")
            .unwrap()
            .get(0)
            .unwrap()
            .get("futureKey"),
        Some(&json!(true))
    );
}

// ---------------------------------------------------------------------------
// routes.rs — GET/PUT /api/config/settings via oneshot
// ---------------------------------------------------------------------------

fn test_context(data_dir: &Path) -> RouterContext {
    let config = ServerConfig {
        port: 0,
        host: None,
        lan: false,
        ui_password: None,
        api_only: false,
        data_dir: data_dir.to_path_buf(),
        dist_dir: data_dir.join("dist"),
        tunnel: Default::default(),
        engine: EngineConfig::Managed {
            hostname: "127.0.0.1".to_string(),
        },
    };
    RouterContext {
        config: Arc::new(config),
        engine: EngineState::external("http://127.0.0.1:1".to_string(), None),
        hub: EventHub::new(),
    }
}

fn request_json(method: &str, uri: &str, body: Option<(&str, &str)>) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some((content, content_type)) = body {
        builder = builder
            .header(header::CONTENT_TYPE, content_type)
            .header(header::CONTENT_LENGTH, content.len());
        builder.body(Body::from(content.to_string())).unwrap()
    } else {
        builder.body(Body::empty()).unwrap()
    }
}

#[tokio::test]
async fn get_settings_serves_migrated_defaults() {
    let dir = temp_dir_unique("route-get");
    let app = router(test_context(&dir));

    let response = app
        .clone()
        .oneshot(request_json("GET", "/api/config/settings", None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(content_type.starts_with("application/json"));
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let payload: Value = serde_json::from_slice(&body).unwrap();
    let payload = payload.as_object().unwrap();
    assert_eq!(payload.get("lightThemeId"), Some(&json!("flexoki-light")));
    assert_eq!(payload.get("collapsibleThinkingBlocks"), Some(&json!(true)));
    assert!(
        dir.join("settings.json").exists(),
        "the GET persists its migrations like the JS"
    );
}

#[tokio::test]
async fn put_settings_persists_and_answers_with_the_formatted_state() {
    let dir = temp_dir_unique("route-put");
    let app = router(test_context(&dir));

    let response = app
        .clone()
        .oneshot(request_json(
            "PUT",
            "/api/config/settings",
            Some((
                r#"{ "themeId": "z", "fontSize": 999, "unknown": 1 }"#,
                "application/json",
            )),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let payload: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(payload.get("fontSize"), Some(&json!(200)));
    assert!(payload.get("unknown").is_none());

    let response = app
        .oneshot(request_json("GET", "/api/config/settings", None))
        .await
        .unwrap();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let payload: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(payload.get("themeId"), Some(&json!("z")));
    assert_eq!(payload.get("fontSize"), Some(&json!(200)));
}

#[tokio::test]
async fn put_settings_rejects_malformed_json_with_400() {
    let dir = temp_dir_unique("route-400");
    let app = router(test_context(&dir));
    let response = app
        .oneshot(request_json(
            "PUT",
            "/api/config/settings",
            Some(("{oops", "application/json")),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn put_settings_without_json_content_type_merges_nothing() {
    let dir = temp_dir_unique("route-text");
    let app = router(test_context(&dir));
    let response = app
        .oneshot(request_json(
            "PUT",
            "/api/config/settings",
            Some(("hello", "text/plain")),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let payload: Value = serde_json::from_slice(&body).unwrap();
    // PUT runs no startup migrations: only the merge defaults materialize.
    assert_eq!(payload.get("securityScopedBookmarks"), Some(&json!([])));
    assert!(
        payload.get("fontSize").is_none(),
        "a non-JSON body merges nothing"
    );
}

#[tokio::test]
async fn settings_routes_map_io_failures_to_500() {
    // A data_dir that is a FILE makes settings.json unwritable.
    let dir = temp_dir_unique("route-500");
    let blocked = dir.join("not-a-dir");
    std::fs::write(&blocked, "x").unwrap();
    let app = router(test_context(&blocked));

    let response = app
        .clone()
        .oneshot(request_json(
            "PUT",
            "/api/config/settings",
            Some((r#"{ "themeId": "x" }"#, "application/json")),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let payload: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        payload.get("error"),
        Some(&json!("Failed to save settings"))
    );

    let response = app
        .oneshot(request_json("GET", "/api/config/settings", None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let payload: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        payload.get("error"),
        Some(&json!("Failed to read settings"))
    );
}

// Keep clippy quiet about the test-only HashMap import pattern.
#[allow(dead_code)]
type UnusedTestMap = HashMap<String, ()>;
