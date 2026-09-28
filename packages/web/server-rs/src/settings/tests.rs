//! Unit tests for the settings module port. Mirrors the JS test suites
//! (`settings-helpers.test.js`, `settings-normalization-runtime.test.js`,
//! `settings-runtime.test.js`) plus route-level behavior via `oneshot`.
//! 中文说明：settings 模块移植的单元测试，覆盖四层——normalization.rs
//! 的纯函数、helpers.rs 的 sanitize/merge/format 三段、runtime.rs 的
//! SettingsStore 读写与磁盘迁移，以及 routes.rs 的 GET/PUT
//! /api/config/settings 路由（通过 tower oneshot 直接驱动 axum Router）。

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

/// 创建以 `tag` 标识的唯一临时目录（路径混入进程 ID 与纳秒时间戳，保证
/// 并行测试互不冲突），返回已创建的目录路径。
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

/// 在唯一临时目录内构造指向 `settings.json` 的 SettingsStore，返回
/// (store, 临时目录)；目录供断言 sibling 文件或磁盘副作用时使用。
fn store_for_temp(tag: &str) -> (Arc<SettingsStore>, PathBuf) {
    let dir = temp_dir_unique(tag);
    let store = store_for_path(&dir.join("settings.json"));
    (store, dir)
}

/// 测试助手：把 `json!` 产生的 Value 以 JSON 对象形式取出（失败即 panic）。
fn object(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap()
}

/// 测试助手：对载荷执行 `sanitize_settings_update` 并 unwrap，
/// 简化各 sanitize 用例的断言前奏。
fn sanitized(payload: Value) -> super::helpers::SanitizedUpdate {
    sanitize_settings_update(&payload).unwrap()
}

// ---------------------------------------------------------------------------
// normalization.rs
// ---------------------------------------------------------------------------

/// 契约：`normalize_directory_path` 先剥离成对引号再做 `~` 展开，
/// 单独出现的不成对引号保持原样。
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

/// 契约：`normalize_path_for_persistence` 对存在的路径解析符号链接，
/// 返回 canonicalize 后的真实路径。
#[test]
fn normalize_path_for_persistence_resolves_symlinks() {
    let dir = temp_dir_unique("norm-symlink");
    let real = dir.join("real");
    std::fs::create_dir_all(&real).unwrap();
    let link = dir.join("link");
    crate::os_compat::symlink(&real, &link).unwrap();

    let normalized = norm::normalize_path_for_persistence(
        &Value::from(link.to_string_lossy().into_owned()),
        true,
    );
    // safe_realpath strips the Windows verbatim prefix, so compare against
    // the same resolution the production path produces.
    assert_eq!(
        normalized.as_str().unwrap(),
        norm::safe_realpath(&real.to_string_lossy())
    );
}

/// 契约：路径不存在时 `normalize_path_for_persistence` 原样返回字符串，
/// 非字符串值完全透传。
#[test]
fn normalize_path_for_persistence_falls_back_when_path_missing() {
    let normalized =
        norm::normalize_path_for_persistence(&Value::from("/definitely/not/here"), true);
    // win32 path.resolve of a root-relative posix-looking path yields
    // backslash-separated segments (Node does the same).
    let expected = if cfg!(windows) {
        "\\definitely\\not\\here"
    } else {
        "/definitely/not/here"
    };
    assert_eq!(normalized.as_str().unwrap(), expected);
    // Non-strings pass through untouched.
    assert_eq!(
        norm::normalize_path_for_persistence(&Value::from(42), true),
        Value::from(42)
    );
}

/// 契约：`sanitize_projects` 按 id 与 path 去重，并逐字段校验——label
/// trim、图标背景必须是十六进制色值、模型引用必须带 provider、非数字
/// 或负数时间戳被丢弃；非法字段缺失而非整体失败。
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

/// 契约：`normalize_string_array` 只保留非空字符串、去重且维持首次出现
/// 顺序；非数组输入得到空数组。
#[test]
fn normalize_string_array_dedupes_and_keeps_order() {
    let value = json!(["b", "a", "b", "", 3, "a"]);
    assert_eq!(norm::normalize_string_array(&value), vec!["b", "a"]);
    assert!(norm::normalize_string_array(&json!("not-an-array")).is_empty());
}

/// 契约：managed tunnel 主机名归一化从任意带协议/路径的形式提取纯主机名
/// 并转小写；空白、非字符串、非法 URL 一律返回 None。
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

/// 契约：tunnel 预设按 trim 后的 id 与归一化主机名双重去重，id/name/
/// hostname 全部 trim；非数组输入返回 None。
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

/// 契约：preset token 映射对键值分别 trim 并剔除空键空值；全部无效或
/// 输入非对象时返回 None。
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

/// 契约：typographySizes 部分更新只保留已知且非空的档位键；清洗结果
/// 为空或无输入时返回 None（不写键）。
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

/// 契约：模型引用列表 trim 后按 provider/model 去重并遵守传入上限，
/// 缺字段或非对象条目被剔除；非数组输入返回 None。
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

/// 契约：skill catalog 只保留具备必填 id/label/source 的条目并按 id
/// 去重，可选字段 trim 后保留。
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

/// 契约：tunnel 两个 TTL 的钳制边界使用生产常量——bootstrap 夹在
/// 60s–24h、默认 30min；session 夹在 5min–30d、默认 8h；null 透传为 null。
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

/// 契约：tunnel provider/mode 未知或缺失时回落默认值（cloudflare /
/// quick），合法值 trim 且大小写不敏感。
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

/// 契约：`normalize_optional_path` 展开 `~` 后必须落在 home 目录内，
/// 越界路径返回错误（与 JS 抛出的 TunnelServiceError 对齐）。
#[test]
fn optional_tunnel_path_must_stay_in_home() {
    let inside = norm::normalize_optional_path("~/tunnel/config.yml").unwrap();
    let home = norm::home_dir().unwrap();
    assert_eq!(Path::new(&inside), home.join("tunnel/config.yml"));
    let err = norm::normalize_optional_path("/etc/passwd").unwrap_err();
    assert!(err.to_string().contains("within the home directory"));
}

/// 契约：项目 id 由规范化路径确定性生成——`path_` 前缀 + URL-safe
/// base64（无填充），正反斜杠与尾斜杠等价，空白路径得到空串。
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

/// 契约：`sha1_hex` 与公开标准测试向量一致（"abc" 与空串）。
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

/// 契约：`path_resolve` / `path_relative` 做纯词法规范化——相对路径基于
/// cwd、`..` 与 `.` 被消解，`path_relative` 计算两路径间的相对路径。
#[test]
fn path_resolve_lexically_normalizes() {
    let cwd = std::env::current_dir().unwrap();
    assert_eq!(norm::path_resolve("x/y"), cwd.join("x/y"));
    assert_eq!(norm::path_resolve("/a/b/../c"), PathBuf::from("/a/c"));
    assert_eq!(norm::path_resolve("/a/./b"), PathBuf::from("/a/b"));
    let sep = if cfg!(windows) { "\\" } else { "/" };
    assert_eq!(norm::path_relative("/a/b", "/a/b/c/d"), format!("c{sep}d"));
    assert_eq!(
        norm::path_relative("/a/b/c", "/a/x"),
        format!("..{sep}..{sep}x")
    );
    assert_eq!(norm::path_relative("/a/b", "/a/b"), "");
}

#[test]
fn path_resolve_is_idempotent() {
    let once = norm::path_resolve("a/b/../c");
    let twice = norm::path_resolve(&once.to_string_lossy());
    assert_eq!(once, twice);
}

#[cfg(windows)]
#[test]
fn path_resolve_does_not_duplicate_windows_drive() {
    // Regression: an absolute drive path gained one extra "C:\" per resolve,
    // so every settings save compounded the prefix ("C:\C:\…\Users\…") and
    // the base64 project ids grew past MAX_PATH until the write 500'd.
    assert_eq!(
        norm::path_resolve(r"C:\Users\reamd\Documents"),
        PathBuf::from(r"C:\Users\reamd\Documents")
    );
    // Only the drive letter uppercases (path.resolve("c:\\x") === "C:\\x");
    // later components keep their case, as in Node.
    assert_eq!(
        norm::path_resolve(r"c:\users\reamd"),
        PathBuf::from(r"C:\users\reamd")
    );
}

#[cfg(windows)]
#[test]
fn normalize_directory_path_collapses_repeated_drive_prefixes() {
    // Self-heal for settings persisted by the drive-duplication bug.
    assert_eq!(
        norm::normalize_directory_path(r"C:\C:\C:\Users\reamd"),
        r"C:\Users\reamd"
    );
    // Mixed separators and single prefixes are untouched.
    assert_eq!(
        norm::normalize_directory_path(r"C:/C:\Users\reamd"),
        r"C:\Users\reamd"
    );
    assert_eq!(
        norm::normalize_directory_path(r"C:\Users\reamd"),
        r"C:\Users\reamd"
    );
}

#[cfg(windows)]
#[test]
fn safe_realpath_strips_verbatim_prefix() {
    // Node's realpathSync has no "\\?\" prefix; Rust's canonicalize does.
    let dir = temp_dir_unique("verbatim");
    std::fs::create_dir_all(&dir).unwrap();
    let resolved = norm::safe_realpath(dir.to_string_lossy().as_ref());
    assert!(!resolved.starts_with(r"\\?\"), "{resolved}");
}

#[cfg(windows)]
#[test]
fn sanitize_projects_heals_corrupted_drive_prefix() {
    // End to end: a project entry persisted with the compounded prefix
    // resolves back to the real directory and its id is recomputed.
    let dir = temp_dir_unique("heal-project");
    std::fs::create_dir_all(&dir).unwrap();
    let real = dir.to_string_lossy().into_owned();
    let corrupted = format!(r"C:\C:\C:\{real}");
    let input = json!([{
        "id": norm::create_project_id_from_path(&corrupted),
        "path": corrupted,
    }]);
    let healed = norm::sanitize_projects(Some(&input)).unwrap();
    // sanitize heals the path but keeps the stored id; the id remap to the
    // canonical form happens in migrate_settings_to_deterministic_project_ids
    // (covered end to end below).
    assert_eq!(
        healed[0]["path"].as_str().unwrap(),
        norm::safe_realpath(&real)
    );
    assert_eq!(
        healed[0]["id"].as_str().unwrap(),
        input[0]["id"].as_str().unwrap()
    );
}

/// End-to-end heal: settings persisted by the drive-duplication bug (and the
/// registry files keyed by the corrupted base64 ids) must self-heal on the
/// next read_migrated — collapsed paths, canonical ids, migrated storage.
#[cfg(windows)]
#[tokio::test]
async fn read_migrated_heals_corrupted_drive_prefixes_end_to_end() {
    let root = temp_dir_unique("heal-e2e");
    std::fs::create_dir_all(root.join("projects")).unwrap();
    let real = root.join("repo");
    std::fs::create_dir_all(&real).unwrap();
    let real_str = real.to_string_lossy().into_owned();
    let corrupted_path = format!(r"C:\C:\C:\{real_str}");
    let corrupted_id = norm::create_project_id_from_path(&corrupted_path);
    let clean_id = norm::create_project_id_from_path(&real_str);

    let settings = json!({
        "lastDirectory": corrupted_path,
        "activeProjectId": corrupted_id,
        "projects": [{ "id": corrupted_id, "path": corrupted_path, "label": "heal" }],
    });
    std::fs::write(
        root.join("settings.json"),
        serde_json::to_string_pretty(&settings).unwrap(),
    )
    .unwrap();
    // Registry file under the corrupted id — must migrate to the clean id.
    std::fs::write(
        root.join("projects").join(format!("{corrupted_id}.json")),
        r#"{"projectPath":"stale"}"#,
    )
    .unwrap();

    let store = SettingsStore::new(root.join("settings.json"));
    let healed = store.read_migrated().await.unwrap();

    let expected_real = norm::safe_realpath(&real_str);
    assert_eq!(healed["lastDirectory"].as_str().unwrap(), expected_real);
    let projects = healed["projects"].as_array().unwrap();
    assert_eq!(projects[0]["path"].as_str().unwrap(), expected_real);
    assert_eq!(projects[0]["id"].as_str().unwrap(), clean_id);
    assert_eq!(healed["activeProjectId"].as_str().unwrap(), clean_id);

    let migrated = root.join("projects").join(format!("{clean_id}.json"));
    let stale = root.join("projects").join(format!("{corrupted_id}.json"));
    assert!(migrated.exists(), "registry not migrated to clean id");
    assert!(!stale.exists(), "corrupted registry file not removed");
    let body: Value = serde_json::from_str(&std::fs::read_to_string(&migrated).unwrap()).unwrap();
    assert_eq!(body["projectPath"].as_str().unwrap(), expected_real);

    // Idempotent: a second pass changes nothing.
    let again = store.read_migrated().await.unwrap();
    assert_eq!(again, healed);
    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// helpers.rs — sanitizeSettingsUpdate
// ---------------------------------------------------------------------------

/// 契约：布尔设置字段只接受真布尔；字符串 "true"、数字、null 一律丢弃。
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

/// 契约：尺寸类数值按 JS 语义 round 后夹到各自 [min, max] 区间。
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

/// 契约：terminalShell 与 terminalLoginShells trim + 转小写后按白名单
/// 校验，login shells 额外去重，非法项被剔除。
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

/// 契约：枚举字段命中白名单原样保留、白名单之外的值整键丢弃；旧值
/// desktopWindowControlsPosition "auto" 迁移为 "right"。
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

/// 契约：permissionAutoAccept 只保留布尔 session 开关（非布尔条目剔除），
/// revision 必须是非负安全整数，缺省为 0。
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

/// 契约：desktopUiPassword 仅 trim（空串保留以支持清除密码）；
/// shortcutOverrides 剔除空键/空值/非字符串值，空对象保留用于整体重置。
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

/// 契约：模型列表合法输入原样往返、缺字段的条目整体剔除、非数组输入
/// 整键丢弃；collapsedModelProviders 等字符串数组类字段去重保序。
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

/// 契约：recentEfforts 对每个模型键的档位数组去重；非对象、非数组值、
/// 空键、空数组等非法形状整键丢弃。
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

/// 契约：未识别的键与已退役的旧键（update 通知类）在清洗时全部静默丢弃。
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

/// 契约：可清空的字符串字段传空白串时映射为 `None`（等价 JS 的
/// undefined 赋值），表示清除已持久化的值。
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

/// 契约：tunnel 字段清洗组合——TTL 钳制、provider/mode 回落默认、主机名
/// 归一化、token trim、null 显式清除；config path 越界 home 目录时报错。
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

/// 契约：主题字段——themeId 非空即保留，themeVariant 只认 light/dark，
/// splash 颜色 trim 后非空才保留。
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

/// 契约：STT 旧 provider 名迁移（server→openai-compatible 等）、URL trim、
/// followUpBehavior 旧值 immediate→steer 及 queueModeEnabled 兜底；
/// pwa 字段归一化失败映射为清除。
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

/// 契约：merge 语义——Some 覆盖、None 清除、未提及键保留；
/// bookmarks 强制去重重建（可为空数组），typographySizes 按键级合并。
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

/// 契约：formatSettingsResponse 绝不回传 token 明文（只回 has 标志），
/// 并为布尔/集合/PWA 字段补齐默认值；desktop 专属字段仅在
/// OMPCHAMBER_RUNTIME=desktop 时出现。
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

/// 契约：settings.json 缺失时 read_migrated 生成迁移后的默认值（主题、
/// 通知开关与模板等）并落盘，再次读取结果稳定。
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

/// 契约：严格读取把损坏的 JSON 视为解析错误；宽松读取（GET 使用）则
/// 告警并从迁移默认值重新开始。
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

/// 契约：严格读取把非对象 JSON（如数组）判为 malformed 错误。
#[tokio::test]
async fn non_object_payload_is_an_error_for_strict_reads() {
    let (store, _dir) = store_for_temp("nonobject");
    tokio::fs::write(store.settings_path(), "[1,2]")
        .await
        .unwrap();
    let err = store.read_strict().await.unwrap_err();
    assert!(err.to_string().contains("malformed"));
}

/// 契约：persist 先清洗再写盘并返回格式化响应；未知键丢弃，后续更新按
/// 合并语义保留先前字段。
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

/// 契约：空字符串清除 defaultModel——先持久化一个值，再用 "" 清空后
/// 该键从磁盘状态中消失。
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

/// 契约：原子写——内容以 pretty JSON 落盘，不留临时文件、不动 sibling；
/// Unix 下文件模式 0600、目录 0700。
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

/// 契约：read_migrated 顺带清理历史遗留的 settings.json.tmp-* 临时文件，
/// 其余无关文件不受影响。
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

/// 契约：迁移保留未知字段（前向兼容）、删除退役键，并把旧 namedTunnel
/// 字段迁移为 managedRemoteTunnel 等价物；后续 persist 不丢未知字段。
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

/// 契约：旧版随机项目 id 迁移为路径确定性 id——settings 与项目计划文件
/// 改写、存储目录重命名，计划文件中指向旧目录的路径同步重映射，
/// sibling 目录不受影响。
#[tokio::test]
async fn deterministic_project_ids_remap_plan_files() {
    let dir = temp_dir_unique("det-ids");
    let project_path = dir.join("project");
    std::fs::create_dir_all(&project_path).unwrap();
    let canonical_project =
        norm::strip_verbatim_prefix(std::fs::canonicalize(&project_path).unwrap());
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
                .join("plans")
                .join("inside.md")
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

/// 契约：退役的 collapsedProjects 列表迁移为各项目条目上的
/// sidebarCollapsed 布尔标记。
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

/// 契约：只有旧版 lastDirectory 时，迁移以其 canonical 路径播种一个
/// 项目并设为 activeProjectId。
#[tokio::test]
async fn legacy_last_directory_seeds_a_project() {
    let (store, dir) = store_for_temp("lastdir");
    let workspace = dir.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let canonical = norm::strip_verbatim_prefix(std::fs::canonicalize(&workspace).unwrap());
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

/// 契约：persist 校验项目目录存在性——目录已消失的项目被剔除，并把
/// activeProjectId 收敛到幸存项目。
#[tokio::test]
async fn persist_drops_projects_whose_directory_is_gone() {
    let (store, _dir) = store_for_temp("validate");
    let live_dir = store.settings_path().parent().unwrap().join("live");
    std::fs::create_dir_all(&live_dir).unwrap();
    let canonical_live = norm::strip_verbatim_prefix(std::fs::canonicalize(&live_dir).unwrap());
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

/// 契约：类型化视图 read_typed 把未知字段捕获进 extra（含嵌套的 project
/// 条目），序列化后未知字段原样往返不丢失。
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

/// 构造路由测试用的 RouterContext——端口 0、外部 EngineState 与独立
/// EventHub，data_dir 指向给定临时目录。
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

/// 构造 oneshot 用的 HTTP 请求；body 为 Some((内容, Content-Type)) 时附带
/// 相应请求头与长度，否则发送空 body。
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

/// 契约：GET /api/config/settings 首次访问返回迁移后的默认值
/// （application/json），并像 JS 一样把迁移结果落盘。
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

/// 契约：PUT 合法 JSON 后返回格式化状态（越界值被钳制、未知键丢弃），
/// 随后的 GET 能读到同一持久化结果。
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

/// 契约：PUT 收到损坏的 JSON body 时以 400 拒绝。
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

/// 契约：PUT 非 JSON Content-Type 的 body 合并不到任何设置，仅回落
/// merge 默认字段（如空 bookmarks 数组）。
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

/// 契约：data_dir 不可写（被文件占位）时 PUT/GET 分别映射为 500，错误
/// 消息为 "Failed to save settings" / "Failed to read settings"。
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
/// 仅为压制 clippy 对测试专用 HashMap 导入模式的告警，本身无实际用途。
#[allow(dead_code)]
type UnusedTestMap = HashMap<String, ()>;
