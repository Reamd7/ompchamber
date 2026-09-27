//! Port of `opencode/opencode-resolution-runtime.js`
//! (`getOpenCodeResolutionSnapshot`) backed by the `env-runtime.js` subset it
//! needs (`resolveOpencodeCliPath`, `resolveNodeCliPath`, `resolveBunCliPath`,
//! `resolveManagedOpenCodeLaunchSpec`).
//!
//! `resolveOpencodeCliPath` resolves the RUNTIME that launches the managed omp
//! host (Bun), not an `opencode` CLI — the historical name is kept because the
//! resolution snapshot flows through it. The Rust engine does not mutate
//! `OPENCODE_BINARY` after startup, so the "detected now" correction dance the
//! JS performs (restore the previous source, re-detect, un-shadow env-set
//! values) collapses to a single fresh detection.
//!
//! 中文说明：本模块负责解析启动 omp host（托管引擎进程）所需的运行时与
//! 启动参数，产出与 JS 端 `getOpenCodeResolutionSnapshot` 同构的快照，并由
//! `opencode_resolution` 路由对外提供。解析顺序为：显式环境变量 → PATH
//! 查找 → 常见安装位置回退；结果经 `LazyLock` 按进程生命周期缓存，与
//! JS 端模块级变量"只解析一次"的语义保持一致。

use std::path::{Path, PathBuf};

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::context::RouterContext;

/// Engine runtime resolution snapshot (`getOpenCodeResolutionSnapshot` shape).
#[derive(Clone)]
pub(crate) struct EngineResolution {
    /// `resolvedOpencodeBinary` (the omp-host runtime, usually `bun`).
    pub resolved: Option<PathBuf>,
    /// `resolvedOpencodeBinarySource`: `env` | `path` | `fallback`.
    pub source: Option<&'static str>,
    /// `launchBinary`：实际执行的二进制（Windows 的 `.cmd`/`.bat` shim 会换成 cmd.exe）。
    pub launch_binary: Option<String>,
    /// `launchArgs`：传给 `launch_binary` 的参数（cmd 包装时为 `/d /s /c call <bin>`）。
    pub launch_args: Vec<String>,
    /// `launchWrapperType`：启动包装类型标记；目前仅 `"cmd-wrapper"`，其余为 None。
    pub launch_wrapper_type: Option<&'static str>,
    /// `node`：EnvRuntime 中懒解析缓存的 node 二进制路径。
    pub node: Option<PathBuf>,
    /// `bun`：EnvRuntime 中懒解析缓存的 bun 二进制路径。
    pub bun: Option<PathBuf>,
}

/// `EngineResolution` 的解析入口与 JSON 序列化实现。
impl EngineResolution {
    /// JS semantics: the binary-resolution snapshot lives in module-level
    /// variables resolved once (env-runtime `resolvedOpencodeBinary` etc.) —
    /// /health and the resolution route read the CACHED values, they do not
    /// re-scan PATH per request.
    pub(crate) fn resolve() -> Self {
        // 进程级缓存：对应 JS 的模块级变量，首次访问时探测一次，之后复用。
        static CACHED: std::sync::LazyLock<EngineResolution> =
            std::sync::LazyLock::new(EngineResolution::resolve_uncached);
        (*CACHED).clone()
    }

    /// 不走缓存的完整探测：解析 omp host 运行时、读取 EnvRuntime 缓存的
    /// node/bun 状态，并基于解析结果推导启动 spec。仅在 `LazyLock` 首次
    /// 初始化时执行一次。
    fn resolve_uncached() -> Self {
        let (resolved, source) = resolve_omp_host_runtime(
            std::env::var("OMPCHAMBER_OMP_HOST_RUNTIME").ok().as_deref(),
            std::env::var("OPENCODE_BINARY").ok().as_deref(),
        );
        // JS reports the lazy `resolvedNodeBinary` state (shim paths only).
        let node = crate::engine_env::EnvRuntime::shared()
            .resolved_node_binary()
            .map(PathBuf::from);
        // JS reports the lazy `resolvedBunBinary` state (only the shim-runtime
        // paths populate it), never an eager PATH lookup.
        let bun = crate::engine_env::EnvRuntime::shared()
            .resolved_bun_binary()
            .map(PathBuf::from);
        let (launch_binary, launch_args, launch_wrapper_type) = resolved
            .as_deref()
            .map(managed_launch_spec)
            .unwrap_or((None, Vec::new(), None));
        Self {
            resolved,
            source,
            launch_binary,
            launch_args,
            launch_wrapper_type,
            node,
            bun,
        }
    }

    /// `resolved` 的字符串形式（路径有损转换为 UTF-8），供 JSON 输出使用。
    pub(crate) fn resolved_display(&self) -> Option<String> {
        self.resolved.as_ref().map(|path| path_to_string(path))
    }

    /// `node` 的字符串形式，供 JSON 输出使用。
    pub(crate) fn node_display(&self) -> Option<String> {
        self.node.as_ref().map(|path| path_to_string(path))
    }

    /// `bun` 的字符串形式，供 JSON 输出使用。
    pub(crate) fn bun_display(&self) -> Option<String> {
        self.bun.as_ref().map(|path| path_to_string(path))
    }

    /// `GET /api/config/opencode-resolution` body.
    fn snapshot_json(&self) -> serde_json::Value {
        serde_json::json!({
            "resolved": self.resolved_display(),
            "resolvedDir": self.resolved.as_ref().and_then(|path| path.parent()).map(path_to_string),
            "source": self.source,
            "detectedNow": self.resolved_display(),
            "detectedSourceNow": self.source,
            "launchBinary": self.launch_binary.clone(),
            "launchArgs": self.launch_args.clone(),
            "launchWrapperType": self.launch_wrapper_type,
            "node": self.node_display(),
            "bun": self.bun_display(),
        })
    }
}

/// 路径转有损 UTF-8 字符串（无效字节替换为 U+FFFD），避免 JSON 序列化失败。
fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// `stripWrappingQuotes` from `env-runtime.js`.
fn strip_wrapping_quotes(value: &str) -> &str {
    let trimmed = value.trim();
    if trimmed.len() >= 2
        && ((trimmed.starts_with('"') && trimmed.ends_with('"'))
            || (trimmed.starts_with('\'') && trimmed.ends_with('\'')))
    {
        return trimmed[1..trimmed.len() - 1].trim();
    }
    trimmed
}

/// 判断路径是否可执行：必须是常规文件，且 Unix 上任一执行位已置位；
/// 非 Unix 平台只检查是否为常规文件。
fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
use crate::os_compat::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// `searchPathFor`: first existing regular file named `binary` on `PATH`.
fn search_path_for(binary: &str) -> Option<PathBuf> {
    let path = std::env::var("PATH").ok()?;
    let separator = if cfg!(windows) { ';' } else { ':' };
    path.split(separator)
        .filter(|entry| !entry.is_empty())
        .map(|dir| Path::new(dir).join(binary))
        .find(|candidate| candidate.is_file())
}

/// 取用户主目录；统一转发到 `crate::config::home_dir()`。
fn home_dir() -> Option<PathBuf> {
    crate::config::home_dir()
}

/// `resolveOpencodeCliPath`: explicit env runtime → `bun` on PATH →
/// well-known fallback locations. Returns the binary and its source tag.
pub(crate) fn resolve_omp_host_runtime(
    omp_host_runtime: Option<&str>,
    opencode_binary: Option<&str>,
) -> (Option<PathBuf>, Option<&'static str>) {
    let explicit = [omp_host_runtime, opencode_binary]
        .into_iter()
        .flatten()
        .map(strip_wrapping_quotes);
    for candidate in explicit.filter(|value| !value.is_empty()) {
        let path = PathBuf::from(candidate);
        if is_executable(&path) {
            return (Some(path), Some("env"));
        }
    }

    if let Some(found) = search_path_for("bun") {
        return (Some(found), Some("path"));
    }

    let fallbacks: Vec<PathBuf> = if cfg!(windows) {
        home_dir()
            .map(|home| vec![home.join(".bun").join("bin").join("bun.exe")])
            .unwrap_or_default()
    } else {
        vec![
            home_dir()
                .map(|home| home.join(".bun").join("bin").join("bun"))
                .unwrap_or_default(),
            PathBuf::from("/opt/homebrew/bin/bun"),
            PathBuf::from("/usr/local/bin/bun"),
        ]
    };
    for candidate in fallbacks {
        if is_executable(&candidate) {
            return (Some(candidate), Some("fallback"));
        }
    }

    (None, None)
}

/// `resolveNodeCliPath` without the login-shell probe (deferred with the rest
/// of `env-runtime.js`).
pub(crate) fn resolve_node_binary(
    node_binary: Option<&str>,
    ompchamber_node: Option<&str>,
) -> Option<PathBuf> {
    let explicit = [node_binary, ompchamber_node]
        .into_iter()
        .flatten()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty());
    for candidate in explicit.map(PathBuf::from) {
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    if let Some(found) = search_path_for("node") {
        return Some(found);
    }
    let fallbacks = [
        PathBuf::from("/opt/homebrew/bin/node"),
        PathBuf::from("/usr/local/bin/node"),
        PathBuf::from("/usr/bin/node"),
        PathBuf::from("/bin/node"),
    ];
    fallbacks
        .into_iter()
        .find(|candidate| is_executable(candidate))
}

/// `resolveBunCliPath` without the login-shell probe.
pub(crate) fn resolve_bun_binary(
    bun_binary: Option<&str>,
    ompchamber_bun: Option<&str>,
) -> Option<PathBuf> {
    let explicit = [bun_binary, ompchamber_bun]
        .into_iter()
        .flatten()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty());
    for candidate in explicit.map(PathBuf::from) {
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    if let Some(found) = search_path_for("bun") {
        return Some(found);
    }
    let home = home_dir();
    let mut fallbacks = Vec::new();
    if let Some(home) = &home {
        fallbacks.push(home.join(".bun").join("bin").join("bun"));
    }
    fallbacks.extend([
        PathBuf::from("/opt/homebrew/bin/bun"),
        PathBuf::from("/usr/local/bin/bun"),
        PathBuf::from("/usr/bin/bun"),
        PathBuf::from("/bin/bun"),
    ]);
    fallbacks
        .into_iter()
        .find(|candidate| is_executable(candidate))
}

/// `resolveManagedOpenCodeLaunchSpec`: on non-Windows the resolved binary is
/// launched directly. Windows subset: `.cmd`/`.bat` shims go through
/// `cmd.exe`; node_modules/native unwrapping and shebang interpretation stay
/// with the full `env-runtime.js` port.
pub(crate) fn managed_launch_spec(
    resolved: &Path,
) -> (Option<String>, Vec<String>, Option<&'static str>) {
    let binary = path_to_string(resolved);
    if !cfg!(windows) {
        return (Some(binary), Vec::new(), None);
    }
    let ext = resolved
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    if ext == "cmd" || ext == "bat" {
        let comspec = std::env::var("ComSpec")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "cmd.exe".to_string());
        return (
            Some(comspec),
            vec!["/d".into(), "/s".into(), "/c".into(), "call".into(), binary],
            Some("cmd-wrapper"),
        );
    }
    (Some(binary), Vec::new(), None)
}

/// `GET /api/config/opencode-resolution` — engine runtime resolution snapshot.
pub(crate) async fn opencode_resolution(State(_ctx): State<RouterContext>) -> Response {
    let resolution = EngineResolution::resolve();
    (StatusCode::OK, Json(resolution.snapshot_json())).into_response()
}

/// 单元测试：运行时解析的优先级与引号剥离、启动 spec 推导、快照 JSON 的字段契约。
#[cfg(test)]
mod tests {
    use super::*;
    use crate::core_routes::tests::temp_dir;

    /// 验证：显式指定的可执行文件优先胜出（source=`env`），且引号包裹的值会先剥壳再判定。
    #[test]
    fn omp_host_runtime_prefers_explicit_executable_env() {
        let dir = temp_dir("resolution-env");
        let binary = dir.join("custom-runtime");
        std::fs::write(&binary, b"#!/bin/sh\nexit 0\n").expect("write runtime");
        make_executable(&binary);

        let (resolved, source) =
            resolve_omp_host_runtime(Some(binary.to_str().unwrap()), Some("/nonexistent/other"));
        assert_eq!(resolved.as_deref(), Some(binary.as_path()));
        assert_eq!(source, Some("env"));

        // Quote-wrapped values are unwrapped first.
        let wrapped = format!("\"{}\"", binary.display());
        let (resolved, _) = resolve_omp_host_runtime(Some(&wrapped), None);
        assert_eq!(resolved.as_deref(), Some(binary.as_path()));
    }

    /// 验证：显式值不可用时回退到 PATH/常见位置查找——结果要么真实存在，
    /// 要么整体为 None（source 同步为 None），绝不返回不存在的路径。
    #[test]
    fn omp_host_runtime_falls_back_to_path_search() {
        // With no usable explicit values the PATH search decides; on test
        // machines `bun` may or may not exist, so only assert consistency.
        let (resolved, source) =
            resolve_omp_host_runtime(Some("/nonexistent/definitely-missing"), None);
        match resolved {
            Some(path) => assert!(path.is_file(), "resolved runtime must exist: {path:?}"),
            None => assert_eq!(source, None),
        }
    }

    /// 验证：只有成对包裹的引号才被剥除；裸值仅做 trim，空串保持为空。
    #[test]
    fn strip_quotes_only_when_wrapped() {
        assert_eq!(strip_wrapping_quotes("  '/a/b' "), "/a/b");
        assert_eq!(strip_wrapping_quotes("\"/a/b\""), "/a/b");
        assert_eq!(strip_wrapping_quotes(" /a/b "), "/a/b");
        assert_eq!(strip_wrapping_quotes(""), "");
    }

    /// Unix 下为测试产物补上执行位（chmod 0o755），满足 `is_executable` 判定。
    #[cfg(unix)]
    fn make_executable(path: &Path) {
use crate::os_compat::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    /// 非 Unix 平台无执行位概念，空实现。
    #[cfg(not(unix))]
    fn make_executable(_path: &Path) {}

    /// 验证：非 Windows 平台启动 spec 直接使用原二进制，无额外参数与包装类型。
    #[test]
    fn managed_launch_spec_is_direct_on_unix() {
        let spec = managed_launch_spec(Path::new("/usr/local/bin/bun"));
        assert_eq!(spec.0.as_deref(), Some("/usr/local/bin/bun"));
        assert!(spec.1.is_empty());
        assert_eq!(spec.2, None);
    }

    /// 验证：快照 JSON 与 JS 端字段集逐一对齐（resolved/resolvedDir/source/
    /// detectedNow/detectedSourceNow/launch 系列/node/bun）。
    #[test]
    fn resolution_snapshot_has_js_field_set() {
        let resolution = EngineResolution {
            resolved: Some(PathBuf::from("/usr/local/bin/bun")),
            source: Some("path"),
            launch_binary: Some("/usr/local/bin/bun".to_string()),
            launch_args: Vec::new(),
            launch_wrapper_type: None,
            node: Some(PathBuf::from("/usr/local/bin/node")),
            bun: Some(PathBuf::from("/usr/local/bin/bun")),
        };
        let body = resolution.snapshot_json();
        assert_eq!(body["resolved"], "/usr/local/bin/bun");
        assert_eq!(body["resolvedDir"], "/usr/local/bin");
        assert_eq!(body["source"], "path");
        assert_eq!(body["detectedNow"], "/usr/local/bin/bun");
        assert_eq!(body["detectedSourceNow"], "path");
        assert_eq!(body["launchBinary"], "/usr/local/bin/bun");
        assert_eq!(body["launchArgs"], serde_json::json!([]));
        assert!(body["launchWrapperType"].is_null());
        assert_eq!(body["node"], "/usr/local/bin/node");
        assert_eq!(body["bun"], "/usr/local/bin/bun");
    }

    /// 验证：路由 `/api/config/opencode-resolution` 返回 200，且响应包含
    /// 全部快照字段（`launchArgs` 为数组）。
    #[tokio::test]
    async fn opencode_resolution_route_returns_snapshot() {
        use crate::core_routes::router;
        use crate::core_routes::tests::{json_response, test_ctx};
        use crate::engine::EngineState;

        let ctx = test_ctx(
            temp_dir("resolution-route"),
            EngineState::external("http://127.0.0.1:1".into(), None),
        );
        let (status, body) = json_response(
            router(ctx),
            axum::http::Request::builder()
                .uri("/api/config/opencode-resolution")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        for field in [
            "resolved",
            "resolvedDir",
            "source",
            "detectedNow",
            "detectedSourceNow",
            "launchBinary",
            "launchArgs",
            "launchWrapperType",
            "node",
            "bun",
        ] {
            assert!(body.get(field).is_some(), "missing field {field} in {body}");
        }
        assert!(body["launchArgs"].is_array());
    }
}
