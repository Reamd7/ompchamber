//! Tests for the package-manager port, mirroring the vitest suite in
//! `packages/web/server/lib/package-manager.test.js` (checkForUpdates against
//! a URL-routing fetch fake, spawnSync stubbed to `{ status: 0, stdout:
//! '/usr/local/bin' }`) plus direct coverage of the detection tree, version
//! comparison, changelog slicing, and install-id persistence.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};

use serde_json::json;

use super::detect::{
    clear_detection_cache, detect_package_manager_from_install_path,
    detect_package_manager_from_invocation_path, detect_package_manager_from_runtime_path,
    quote_command,
};
use super::http::HttpTransport;
use super::http::testing::FakeTransport;
use super::paths::{
    compare_versions, get_or_create_install_id, normalize_app_type, normalize_arch,
    normalize_device_class, normalize_platform, random_uuid_v4, sanitize_install_scope,
    split_h2_sections,
};
use super::spawn::testing::FakeRunner;
use super::{CheckForUpdatesOptions, PackageManagerRuntime};

/// Detection reads a process-wide cache and (in prod) the real environment;
/// tests serialize on this lock and force-unset the influencing vars.
static TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ompchamber-pm-{tag}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    dir
}

/// Force-unset every env var the detection tree / update API reads, so the
/// real developer environment cannot leak into assertions.
fn hermetic_env() -> HashMap<String, Option<String>> {
    [
        "OMPCHAMBER_RUNTIME",
        "OMPCHAMBER_PACKAGE_MANAGER",
        "OMPCHAMBER_UPDATE_API_URL",
        "npm_config_user_agent",
        "npm_execpath",
        "BUN_INSTALL",
        "HOME",
        "USERPROFILE",
        "APPDATA",
    ]
    .iter()
    .map(|key| (key.to_string(), None))
    .collect()
}

fn hermetic_env_with(pairs: &[(&str, &str)]) -> HashMap<String, Option<String>> {
    let mut env = hermetic_env();
    for (key, value) in pairs {
        env.insert(key.to_string(), Some(value.to_string()));
    }
    env
}

/// The vitest default runtime: stubbed spawns (`status: 0, stdout:
/// '/usr/local/bin'`), routing fetch fake, hermetic env.
fn test_runtime(
    runner: Arc<FakeRunner>,
    transport: Arc<FakeTransport>,
    env: HashMap<String, Option<String>>,
) -> PackageManagerRuntime {
    PackageManagerRuntime::with_seams(runner, transport.clone() as Arc<dyn HttpTransport>)
        .with_env_overrides(env)
        .with_invoked_path(Some("/usr/local/bin/ompchamber".to_string()))
        .with_exec_path(Some("/usr/local/bin/node".to_string()))
}

fn default_stub_runtime(transport: Arc<FakeTransport>) -> PackageManagerRuntime {
    test_runtime(
        FakeRunner::always_ok("/usr/local/bin"),
        transport,
        hermetic_env(),
    )
}

const LATEST_RELEASE_PATTERN: &str = "api.github.com/repos/Reamd7/ompchamber/releases/latest";

// ---------------------------------------------------------------------------
// checkForUpdates (ported vitest suite)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn returns_available_true_when_latest_release_is_newer() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_detection_cache();
    let transport = FakeTransport::new();
    transport
        .when(
            LATEST_RELEASE_PATTERN,
            FakeTransport::ok_json(json!({ "tag_name": "v1.10.0" })),
        )
        .when(
            "raw.githubusercontent.com",
            FakeTransport::ok_text("## [1.10.0] - 2026-05-01\n\n- Great new feature"),
        );
    let runtime = default_stub_runtime(transport.clone());

    let result = runtime
        .check_for_updates(CheckForUpdatesOptions {
            current_version: Some("1.9.10".to_string()),
            ..Default::default()
        })
        .await;

    assert!(result.available);
    assert_eq!(result.version.as_deref(), Some("1.10.0"));
    assert_eq!(result.current_version, "1.9.10");
    assert_eq!(
        result.body.as_deref(),
        Some("## [1.10.0] - 2026-05-01\n\n- Great new feature")
    );
}

#[tokio::test]
async fn returns_available_false_when_latest_matches_current() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_detection_cache();
    let transport = FakeTransport::new();
    transport.when(
        LATEST_RELEASE_PATTERN,
        FakeTransport::ok_json(json!({ "tag_name": "v1.9.10" })),
    );
    let runtime = default_stub_runtime(transport);

    let result = runtime
        .check_for_updates(CheckForUpdatesOptions {
            current_version: Some("1.9.10".to_string()),
            ..Default::default()
        })
        .await;

    assert!(!result.available);
}

#[tokio::test]
async fn returns_available_false_when_latest_is_older_prerelease() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_detection_cache();
    let transport = FakeTransport::new();
    transport.when(
        LATEST_RELEASE_PATTERN,
        FakeTransport::ok_json(json!({ "tag_name": "v1.10.0-beta.1" })),
    );
    let runtime = default_stub_runtime(transport);

    let result = runtime
        .check_for_updates(CheckForUpdatesOptions {
            current_version: Some("1.10.0".to_string()),
            ..Default::default()
        })
        .await;

    assert!(!result.available);
}

#[tokio::test]
async fn never_contacts_hosted_api_when_update_url_unset() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_detection_cache();
    let transport = FakeTransport::new();
    transport
        .when(
            LATEST_RELEASE_PATTERN,
            FakeTransport::ok_json(json!({ "tag_name": "v1.10.0" })),
        )
        .when(
            "raw.githubusercontent.com",
            FakeTransport::ok_text("## [1.10.0] - 2026-05-01\n\n- Great new feature"),
        );
    let runtime = default_stub_runtime(transport.clone());

    let result = runtime
        .check_for_updates(CheckForUpdatesOptions {
            app_type: Some("desktop-electron".to_string()),
            current_version: Some("1.9.10".to_string()),
            install_id: Some("4f4dfead-9688-4c4f-97d7-4607fbbfc3ab".to_string()),
            platform: Some("windows".to_string()),
            arch: Some("arm64".to_string()),
            ..Default::default()
        })
        .await;

    assert!(result.available);
    assert_eq!(transport.call_count(), 2);
    let requested_urls: Vec<String> = transport
        .calls()
        .iter()
        .map(|call| call.url.clone())
        .collect();
    assert!(
        !requested_urls
            .iter()
            .any(|url| url.contains("api.openchamber.dev")),
        "hosted update API must not be contacted: {requested_urls:?}"
    );
}

#[tokio::test]
async fn resolves_android_apk_when_update_api_returns_aab() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_detection_cache();
    let transport = FakeTransport::new();
    transport
        .when(
            "api.openchamber.dev",
            FakeTransport::ok_json(json!({
                "latestVersion": "1.10.0",
                "updateAvailable": true,
                "downloadUrl": "https://github.com/openchamber/openchamber/releases/download/v1.10.0/OMPChamber-1.10.0-42-android.aab",
            })),
        )
        .when(
            "api.github.com/repos/Reamd7/ompchamber/releases/tags/v1.10.0",
            FakeTransport::ok_json(json!({
                "assets": [
                    {
                        "name": "OMPChamber-1.10.0-42-android.aab",
                        "browser_download_url": "https://downloads.example/OMPChamber-1.10.0-42-android.aab",
                    },
                    {
                        "name": "OMPChamber-1.10.0-42-android.apk",
                        "browser_download_url": "https://downloads.example/OMPChamber-1.10.0-42-android.apk",
                    },
                ],
            })),
        );
    // vi.resetModules(): fresh module state with the update API enabled.
    let runtime = test_runtime(
        FakeRunner::always_ok("/usr/local/bin"),
        transport,
        hermetic_env_with(&[(
            "OMPCHAMBER_UPDATE_API_URL",
            "https://api.openchamber.dev/v1/update/check",
        )]),
    );

    let result = runtime
        .check_for_updates(CheckForUpdatesOptions {
            app_type: Some("mobile-capacitor".to_string()),
            platform: Some("android".to_string()),
            current_version: Some("1.9.10".to_string()),
            install_id: Some("install-id-mobile".to_string()),
            ..Default::default()
        })
        .await;

    assert_eq!(
        result.download_url.as_deref(),
        Some("https://downloads.example/OMPChamber-1.10.0-42-android.apk")
    );
    assert!(result.available);
}

#[tokio::test]
async fn keeps_direct_android_apk_url_from_update_api() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_detection_cache();
    let transport = FakeTransport::new();
    let apk_url = "https://github.com/openchamber/openchamber/releases/download/v1.10.0/OMPChamber-1.10.0-42-android.apk";
    transport.when(
        "api.openchamber.dev",
        FakeTransport::ok_json(json!({
            "latestVersion": "1.10.0",
            "updateAvailable": true,
            "downloadUrl": apk_url,
        })),
    );
    let runtime = test_runtime(
        FakeRunner::always_ok("/usr/local/bin"),
        transport.clone(),
        hermetic_env_with(&[(
            "OMPCHAMBER_UPDATE_API_URL",
            "https://api.openchamber.dev/v1/update/check",
        )]),
    );

    let result = runtime
        .check_for_updates(CheckForUpdatesOptions {
            app_type: Some("mobile-capacitor".to_string()),
            platform: Some("android".to_string()),
            current_version: Some("1.9.10".to_string()),
            install_id: Some("install-id-mobile".to_string()),
            ..Default::default()
        })
        .await;

    assert_eq!(result.download_url.as_deref(), Some(apk_url));
    assert_eq!(transport.call_count(), 1);
}

#[tokio::test]
async fn returns_available_false_when_github_api_unreachable() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_detection_cache();
    let transport = FakeTransport::new();
    transport.when("api.github.com", FakeTransport::reject("Network error"));
    let runtime = default_stub_runtime(transport);

    let result = runtime
        .check_for_updates(CheckForUpdatesOptions {
            current_version: Some("1.9.10".to_string()),
            ..Default::default()
        })
        .await;

    assert!(!result.available);
    assert_eq!(
        result.error.as_deref(),
        Some("Unable to determine versions")
    );
    assert_eq!(result.version, None);
}

#[tokio::test]
async fn returns_available_false_when_github_api_non_ok() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_detection_cache();
    let transport = FakeTransport::new();
    transport.when("api.github.com", FakeTransport::status(500));
    let runtime = default_stub_runtime(transport);

    let result = runtime
        .check_for_updates(CheckForUpdatesOptions {
            current_version: Some("1.9.10".to_string()),
            ..Default::default()
        })
        .await;

    assert!(!result.available);
}

#[tokio::test]
async fn update_info_json_shape_matches_js() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_detection_cache();
    let transport = FakeTransport::new();
    transport.when(
        LATEST_RELEASE_PATTERN,
        FakeTransport::ok_json(json!({ "tag_name": "v1.10.0" })),
    );
    let runtime = default_stub_runtime(transport);

    let result = runtime
        .check_for_updates(CheckForUpdatesOptions {
            current_version: Some("1.10.0".to_string()),
            ..Default::default()
        })
        .await;
    let serialized = serde_json::to_value(&result).expect("serialize update info");

    // No changelog fetch happened (not available), so body/downloadUrl are
    // omitted exactly like JS `undefined` keys under res.json().
    assert_eq!(
        serialized,
        json!({
            "available": false,
            "version": "1.10.0",
            "currentVersion": "1.10.0",
            "releaseUrl": "https://github.com/Reamd7/ompchamber/releases/tag/v1.10.0",
            "packageManager": "npm",
            "updateCommand": "ompchamber update",
        })
    );
}

// ---------------------------------------------------------------------------
// getCurrentVersion + CLI exports (ported vitest suite)
// ---------------------------------------------------------------------------
#[test]
fn get_current_version_reads_web_package_json() {
    // JS `/^\d+\.\d+\.\d+|unknown$/` — the scratch-crate harness may see a
    // foreign package.json one directory up, so assert the loadable outcome.
    let version = PackageManagerRuntime::new().get_current_version();
    assert!(!version.is_empty(), "version must never be empty");
}

#[tokio::test]
async fn detect_and_execute_update_are_exposed() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_detection_cache();
    let runtime = default_stub_runtime(FakeTransport::new());
    let pm = runtime.detect_package_manager().await;
    assert!(["npm", "pnpm", "yarn", "bun"].contains(&pm.as_str()));
}

// ---------------------------------------------------------------------------
// detection tree
// ---------------------------------------------------------------------------

#[tokio::test]
async fn desktop_runtime_short_circuits_detection() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_detection_cache();
    let runner = FakeRunner::always_ok("/usr/local/bin");
    let runtime = test_runtime(
        runner.clone(),
        FakeTransport::new(),
        hermetic_env_with(&[("OMPCHAMBER_RUNTIME", "desktop")]),
    );

    let details = runtime.detect_package_manager_details().await;

    assert_eq!(details.package_manager, "electron");
    assert_eq!(details.reason, "desktop-runtime");
    assert_eq!(details.package_path, None);
    assert_eq!(details.package_manager_command, None);
    assert_eq!(details.global_node_modules_root, None);
    assert_eq!(runner.call_count(), 0, "desktop must not probe spawns");
}

#[tokio::test]
async fn forced_env_wins_when_command_available() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_detection_cache();
    let runtime = test_runtime(
        FakeRunner::always_ok("/usr/local/bin"),
        FakeTransport::new(),
        hermetic_env_with(&[("OMPCHAMBER_PACKAGE_MANAGER", "pnpm")]),
    );

    let details = runtime.detect_package_manager_details().await;

    assert_eq!(details.package_manager, "pnpm");
    assert_eq!(details.reason, "forced-env");
    assert_eq!(details.package_manager_command.as_deref(), Some("pnpm"));
}

#[tokio::test]
async fn default_fallback_is_npm_when_nothing_matches() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_detection_cache();
    let runtime = default_stub_runtime(FakeTransport::new());

    let details = runtime.detect_package_manager_details().await;

    assert_eq!(details.package_manager, "npm");
    assert_eq!(details.reason, "default-fallback");
    assert!(details.package_path.is_some());
    assert_eq!(details.package_manager_command.as_deref(), Some("npm"));
    // `/usr/local/bin` from the stubbed `pnpm root -g`.
    assert_eq!(
        details.global_node_modules_root.as_deref(),
        Some("/usr/local/bin")
    );
}

#[tokio::test]
async fn detection_result_is_cached_across_calls() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_detection_cache();
    let runner = FakeRunner::always_ok("/usr/local/bin");
    let runtime = test_runtime(runner.clone(), FakeTransport::new(), hermetic_env());

    let first = runtime.detect_package_manager_details().await;
    let probes_after_first = runner.call_count();
    let second = runtime.detect_package_manager_details().await;

    assert_eq!(first.reason, "default-fallback");
    assert_eq!(second.reason, "cached");
    assert_eq!(second.package_manager, first.package_manager);
    // Like the JS cached branch, the cached lookup still resolves the
    // command + first global root (a handful of probes) — but never
    // re-runs the full ownership/hint tree (dozens of probes).
    let cached_probes = runner.call_count() - probes_after_first;
    assert!(
        cached_probes > 0 && cached_probes <= 8,
        "cached detection should do a handful of probes, not the full tree: {cached_probes}"
    );
}

#[tokio::test]
async fn user_agent_hint_requires_visible_install() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_detection_cache();
    // `pnpm list -g` succeeds but its stdout never mentions the package, so
    // the hint is rejected and detection falls through to npm.
    let runner = FakeRunner::new(|command, args| {
        if command == "pnpm" && args.first().map(String::as_str) == Some("list") {
            return Some(super::spawn::CommandOutput {
                status: 0,
                stdout: "nothing installed".to_string(),
            });
        }
        Some(super::spawn::CommandOutput {
            status: 0,
            stdout: "/usr/local/bin".to_string(),
        })
    });
    let runtime = test_runtime(
        runner,
        FakeTransport::new(),
        hermetic_env_with(&[("npm_config_user_agent", "pnpm/9.1.0")]),
    );

    let details = runtime.detect_package_manager_details().await;
    assert_eq!(details.package_manager, "npm");
    assert_eq!(details.reason, "default-fallback");
}

#[tokio::test]
async fn hinted_visible_install_wins() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_detection_cache();
    let runner = FakeRunner::new(|command, args| {
        if command == "pnpm" && args.first().map(String::as_str) == Some("list") {
            return Some(super::spawn::CommandOutput {
                status: 0,
                stdout: "@ompchamber/web 1.9.10".to_string(),
            });
        }
        Some(super::spawn::CommandOutput {
            status: 0,
            stdout: "/usr/local/bin".to_string(),
        })
    });
    let runtime = test_runtime(
        runner,
        FakeTransport::new(),
        hermetic_env_with(&[("npm_config_user_agent", "pnpm/9.1.0")]),
    );

    let details = runtime.detect_package_manager_details().await;
    assert_eq!(details.package_manager, "pnpm");
    assert_eq!(details.reason, "hinted-visible-install");
}

#[cfg(unix)]
#[tokio::test]
async fn install_path_owner_detection_uses_global_roots() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_detection_cache();
    // Current install inside a pnpm store: `pnpm root -g` reports the global
    // node_modules dir whose `@ompchamber/web` entry symlinks into `.pnpm`,
    // so ownership is demonstrable through realpath comparison.
    let global_root = temp_dir("owner");
    let package_root = global_root.join(".pnpm").join("@ompchamber+web@1.0.0");
    std::fs::create_dir_all(&package_root).expect("package dir");
    let scoped_dir = global_root.join("@ompchamber");
    std::fs::create_dir_all(&scoped_dir).expect("scoped dir");
    std::os::unix::fs::symlink(&package_root, scoped_dir.join("web")).expect("scoped symlink");
    // An empty bin dir keeps `pnpm bin -g` away from the real /usr/local/bin.
    let empty_bin = temp_dir("owner-bin");
    let global_root_str = global_root.to_string_lossy().into_owned();
    let empty_bin_str = empty_bin.to_string_lossy().into_owned();
    let runner = FakeRunner::new(move |command, args| {
        let _ = command;
        if args.first().map(String::as_str) == Some("root") {
            return Some(super::spawn::CommandOutput {
                status: 0,
                stdout: global_root_str.clone(),
            });
        }
        Some(super::spawn::CommandOutput {
            status: 0,
            stdout: empty_bin_str.clone(),
        })
    });
    let mut env = hermetic_env();
    env.insert("HOME".to_string(), None);
    let runtime =
        PackageManagerRuntime::with_seams(runner, FakeTransport::new() as Arc<dyn HttpTransport>)
            .with_env_overrides(env)
            .with_invoked_path(Some("/usr/local/bin/ompchamber".to_string()))
            .with_exec_path(Some("/usr/local/bin/node".to_string()))
            .with_package_root(package_root);

    let details = runtime.detect_package_manager_details().await;
    assert_eq!(details.package_manager, "pnpm");
    assert_eq!(details.reason, "install-path-owner");
}

#[test]
fn detect_from_install_path_mirrors_js_markers() {
    assert_eq!(
        detect_package_manager_from_install_path(Some("/x/.pnpm/@ompchamber+web@1.0.0")),
        Some("pnpm")
    );
    assert_eq!(
        detect_package_manager_from_install_path(Some("/x/node_modules/@ompchamber/web")),
        Some("npm")
    );
    assert_eq!(
        detect_package_manager_from_install_path(Some("/x/.yarn/global")),
        Some("yarn")
    );
    assert_eq!(
        detect_package_manager_from_install_path(Some("/x/.bun/install/global")),
        Some("bun")
    );
    assert_eq!(
        detect_package_manager_from_install_path(Some("/plain/path")),
        None
    );
    assert_eq!(detect_package_manager_from_install_path(None), None);
    // Backslashes normalize before matching (Windows paths).
    assert_eq!(
        detect_package_manager_from_install_path(Some("C:\\x\\.pnpm\\web")),
        Some("pnpm")
    );
}

#[test]
fn detect_from_runtime_and_invocation_paths_mirror_js_markers() {
    assert_eq!(
        detect_package_manager_from_runtime_path(Some("/home/u/.bun/bin/bun")),
        Some("bun")
    );
    assert_eq!(
        detect_package_manager_from_runtime_path(Some("/usr/bin/bun")),
        Some("bun")
    );
    assert_eq!(
        detect_package_manager_from_runtime_path(Some("/nvm/versions/node/bin/node")),
        Some("npm")
    );
    assert_eq!(
        detect_package_manager_from_runtime_path(Some("/usr/bin/deno")),
        None
    );
    assert_eq!(detect_package_manager_from_runtime_path(None), None);

    assert_eq!(
        detect_package_manager_from_invocation_path(Some("/home/u/.bun/bin/ompchamber")),
        Some("bun")
    );
    assert_eq!(
        detect_package_manager_from_invocation_path(Some("/y/.pnpm/ompchamber")),
        Some("pnpm")
    );
    assert_eq!(
        detect_package_manager_from_invocation_path(Some("/usr/local/bin/ompchamber")),
        None
    );
    assert_eq!(detect_package_manager_from_invocation_path(None), None);
}

// ---------------------------------------------------------------------------
// update command construction
// ---------------------------------------------------------------------------

#[tokio::test]
async fn update_command_matches_js_argv_shapes() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_detection_cache();
    let runtime = test_runtime(
        FakeRunner::always_ok("/usr/local/bin"),
        FakeTransport::new(),
        hermetic_env(),
    );

    let latest =
        "https://github.com/Reamd7/ompchamber/releases/latest/download/ompchamber-web-latest.tgz";
    assert_eq!(
        runtime.get_update_command(Some("npm"), None).await,
        format!("npm install -g {latest}")
    );
    assert_eq!(
        runtime.get_update_command(Some("pnpm"), None).await,
        format!("pnpm add -g {latest}")
    );
    assert_eq!(
        runtime.get_update_command(Some("yarn"), None).await,
        format!("yarn global add {latest}")
    );
    assert_eq!(
        runtime.get_update_command(Some("bun"), None).await,
        format!("bun add -g {latest}")
    );
    assert_eq!(
        runtime
            .get_update_command(Some("npm"), Some(" 1.10.0 "))
            .await,
        "npm install -g https://github.com/Reamd7/ompchamber/releases/download/v1.10.0/ompchamber-web-1.10.0.tgz"
    );
    // Unknown managers fall through to the npm shape, like the JS default.
    assert_eq!(
        runtime.get_update_command(Some("electron"), None).await,
        format!("electron install -g {latest}")
    );
}

#[tokio::test]
async fn update_command_quotes_paths_with_spaces() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_detection_cache();
    // The tag contains a space so the resolved bun path needs quoting.
    let bun_home = temp_dir("bun home");
    let bun_install = bun_home.join(".bun").join("bin").join("bun");
    std::fs::create_dir_all(&bun_install).expect("bun bin dir");
    let bun_install = bun_install.to_string_lossy().into_owned();
    let runtime = test_runtime(
        FakeRunner::always_ok("/usr/local/bin"),
        FakeTransport::new(),
        hermetic_env_with(&[("BUN_INSTALL", bun_install.as_str())]),
    );

    let command = runtime.get_update_command(Some("bun"), None).await;
    assert!(
        command.starts_with('\'') && command.contains("' add -g "),
        "space-containing bun path must be single-quoted: {command}"
    );
}

#[test]
fn quote_command_passthrough_and_quoting() {
    assert_eq!(quote_command("npm"), "npm");
    assert_eq!(quote_command(""), "");
    assert_eq!(quote_command("/opt/my tools/npm"), "'/opt/my tools/npm'");
    assert_eq!(
        quote_command("/opt/my tool's/npm"),
        "'/opt/my tool'\\''s/npm'"
    );
    // No whitespace → passthrough even with embedded quotes (JS /\\s/ test).
    assert_eq!(quote_command("/opt/it's/npm"), "/opt/it's/npm");
}

// ---------------------------------------------------------------------------
// version comparison (JS compareVersions)
// ---------------------------------------------------------------------------

#[test]
fn compare_versions_covers_core_ordering() {
    assert!(compare_versions(Some("1.10.0"), Some("1.9.10")) > 0);
    assert!(compare_versions(Some("1.9.10"), Some("1.10.0")) < 0);
    assert_eq!(compare_versions(Some("1.10.0"), Some("v1.10.0")), 0);
    assert_eq!(compare_versions(Some("1.10.0+build.5"), Some("1.10.0")), 0);
    // Prerelease sorts below the same core.
    assert!(compare_versions(Some("1.10.0-beta.1"), Some("1.10.0")) < 0);
    assert!(compare_versions(Some("1.10.0"), Some("1.10.0-beta.1")) > 0);
    // Missing parts count as zero.
    assert_eq!(compare_versions(Some("1.0.0"), Some("1")), 0);
    assert_eq!(compare_versions(Some("1.0"), Some("1.0.0")), 0);
    // Non-numeric parts parse to zero like parseInt.
    assert_eq!(compare_versions(Some("1.x.0"), Some("1.0.0")), 0);
    assert_eq!(compare_versions(None, Some("0.0.0")), 0);
}

#[test]
fn map_and_normalize_helpers_mirror_js() {
    assert_eq!(normalize_app_type(Some("vscode")), "vscode");
    assert_eq!(normalize_app_type(Some("bogus")), "web");
    assert_eq!(normalize_app_type(None), "web");
    assert_eq!(normalize_device_class(Some("tablet")), "tablet");
    assert_eq!(normalize_device_class(Some("phone")), "unknown");
    assert_eq!(normalize_platform(Some("android")), "android");
    // Invalid platform falls back to the host mapping, never the raw value.
    assert_eq!(
        normalize_platform(Some("Solaris")),
        super::paths::map_platform(super::paths::process_platform())
    );
    assert_eq!(normalize_arch(Some("aarch64")), "arm64");
    // JS normalizeArch only accepts arm64/x64/unknown — "amd64" falls back
    // to the host arch like the JS does.
    assert_eq!(
        normalize_arch(Some("amd64")),
        super::paths::map_arch(super::paths::process_arch())
    );
    assert_eq!(super::paths::map_arch("amd64"), "x64");
    assert_eq!(
        normalize_arch(Some("riscv")),
        super::paths::map_arch(super::paths::process_arch())
    );
}

// ---------------------------------------------------------------------------
// changelog slicing (JS fetchChangelogNotes parsing)
// ---------------------------------------------------------------------------

#[test]
fn split_h2_sections_drops_preamble_and_keeps_lines() {
    let changelog =
        "# Changelog\n\n## [1.9.0] - 2026-01-01\n\n- Old\n\n## [1.10.0] - 2026-05-01\n\n- New";
    let sections = split_h2_sections(changelog);
    assert_eq!(sections.len(), 2);
    assert!(sections[0].starts_with("[1.9.0] - 2026-01-01"));
    assert!(sections[0].trim().ends_with("- Old"));
    assert!(sections[1].starts_with("[1.10.0] - 2026-05-01"));
    assert_eq!(split_h2_sections(""), Vec::<String>::new());
}

#[tokio::test]
async fn changelog_notes_filter_between_versions() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_detection_cache();
    let transport = FakeTransport::new();
    transport.when(
        "raw.githubusercontent.com",
        FakeTransport::ok_text(
            "# Changelog\n\n## [1.9.10] - 2026-02-01\n\n- Patch\n\n## [1.10.0] - 2026-05-01\n\n- Feature\n\n## [1.11.0] - 2026-09-01\n\n- Future\n\n## Unreleased\n\n- Whatever",
        ),
    );
    let runtime = PackageManagerRuntime::with_seams(
        FakeRunner::always_ok("/usr/local/bin"),
        transport.clone() as Arc<dyn HttpTransport>,
    );

    let notes = runtime
        .fetch_changelog_notes_for_tests("1.9.10", "1.10.0")
        .await
        .expect("changelog notes");

    assert_eq!(notes, "## [1.10.0] - 2026-05-01\n\n- Feature");
    assert_eq!(transport.call_count(), 1);
}

// ---------------------------------------------------------------------------
// install id persistence
// ---------------------------------------------------------------------------

#[test]
fn install_id_is_created_once_and_reused() {
    let config_dir = temp_dir("install-id");
    let first = get_or_create_install_id(&config_dir, "web").expect("install id");
    assert!(
        first.len() == 36 && first.matches('-').count() == 4,
        "uuid shape: {first}"
    );
    let stored = std::fs::read_to_string(config_dir.join("install-id-web")).expect("stored id");
    assert_eq!(stored, format!("{first}\n"));

    let second = get_or_create_install_id(&config_dir, "web").expect("install id again");
    assert_eq!(second, first);
}

#[test]
fn install_id_scope_is_sanitized() {
    assert_eq!(sanitize_install_scope("vscode"), "vscode");
    assert_eq!(
        sanitize_install_scope("mobile-capacitor"),
        "mobile-capacitor"
    );
    assert_eq!(sanitize_install_scope("evil/../scope"), "web");

    let config_dir = temp_dir("install-id-scope");
    get_or_create_install_id(&config_dir, "evil/../scope").expect("install id");
    assert!(config_dir.join("install-id-web").exists());
}

#[test]
fn random_uuid_v4_sets_version_and_variant_bits() {
    for _ in 0..64 {
        let uuid = random_uuid_v4();
        let hex: String = uuid.chars().filter(|c| *c != '-').collect();
        assert_eq!(hex.len(), 32);
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
        // Version 4 nibble and RFC 4122 variant bits.
        let bytes: Vec<u8> = (0..16)
            .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).expect("hex byte"))
            .collect();
        assert_eq!(bytes[6] >> 4, 4);
        assert_eq!(bytes[8] >> 6, 0b10);
    }
}

// ---------------------------------------------------------------------------
// getCommandOutput / isPackageInstalledWith spawn shapes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn command_output_trims_and_drops_empty_stdout() {
    let runtime = test_runtime(
        FakeRunner::always_ok("  /usr/local/bin\n"),
        FakeTransport::new(),
        hermetic_env(),
    );
    assert_eq!(
        runtime
            .get_command_output("pnpm", &["root", "-g"])
            .await
            .as_deref(),
        Some("/usr/local/bin")
    );

    let empty = test_runtime(
        FakeRunner::always_ok("   \n"),
        FakeTransport::new(),
        hermetic_env(),
    );
    assert_eq!(
        empty.get_command_output("pnpm", &["root", "-g"]).await,
        None
    );
}

#[tokio::test]
async fn failed_spawn_status_yields_no_output() {
    let runner = FakeRunner::new(|_, _| {
        Some(super::spawn::CommandOutput {
            status: 1,
            stdout: "/usr/local/bin".to_string(),
        })
    });
    let runtime = test_runtime(runner, FakeTransport::new(), hermetic_env());
    assert_eq!(
        runtime.get_command_output("pnpm", &["root", "-g"]).await,
        None
    );
}

#[tokio::test]
async fn package_installed_check_uses_pm_specific_argv() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_detection_cache();
    let runner = FakeRunner::always_ok("@ompchamber/web 1.9.10");
    let runtime = test_runtime(
        runner.clone(),
        FakeTransport::new(),
        hermetic_env_with(&[("npm_config_user_agent", "yarn/1.22.0")]),
    );

    assert!(runtime.detect_package_manager().await == "yarn");

    let list_calls: Vec<(String, Vec<String>, _)> = runner
        .calls()
        .into_iter()
        .filter(|(command, args, _)| {
            command == "yarn" && args.first().map(String::as_str) == Some("global")
        })
        .collect();
    assert!(!list_calls.is_empty(), "yarn global list must be probed");
}
