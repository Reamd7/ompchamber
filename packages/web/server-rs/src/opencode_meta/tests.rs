//! Tests for the `opencode_meta` port — core logic plus oneshot route
//! coverage, mirroring `models-metadata.test.js`, `npm-registry.test.js`,
//! `project-icon-routes.test.js`, and `openchamber-routes.test.js`.
//!
//! 中文说明：`opencode_meta` 移植的集成测试，逐点镜像 JS 侧的
//! `models-metadata.test.js`、`npm-registry.test.js`、
//! `project-icon-routes.test.js` 与 `openchamber-routes.test.js`：覆盖
//! 目录缓存分层与 proxy 重试、npm 查询/缓存/去重、图标路由与更新路由的
//! 行为契约。文件下方先定义共享测试 harness（临时目录、fake fetch、fake
//! 包管理器），再按被测模块分组排列各测试。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};

use futures::future::BoxFuture;
use serde_json::{Value, json};
use tower::ServiceExt;

use super::http::{HttpError, HttpFetch, HttpRequest, HttpResponse};
use super::models_metadata::{
    self, CatalogIo, ModelsMetadataCache, ProxyEndpoint, env_proxies_from, parse_scutil_output,
};
use super::npm_registry::{self, NpmInfo, NpmIo, NpmStatus};
use super::openchamber_routes::{
    InstallDecision, infer_device_class, install_decision, parse_report_usage,
    resolve_systemd_service_unit,
};
use super::parse_query;
use super::project_icon_routes::apply_project_icon_svg_theme;
use crate::config::{EngineConfig, ServerConfig};
use crate::context::RouterContext;
use crate::engine::EngineState;
use crate::hub::EventHub;
use crate::package_manager::PackageManagerRuntime;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// 创建进程内唯一的临时目录（前缀 + PID + 随机数），避免测试间互相干扰。
fn temp_dir_unique(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "opencode-meta-{prefix}-{}-{:x}",
        std::process::id(),
        rand::random::<u64>()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// 构造指向给定数据目录的 `RouterContext`：固定端口 7897、不可达的引擎
/// 地址与全新 `EventHub`，只填路由挂载所需的最低字段。
fn test_context(data_dir: &Path) -> RouterContext {
    let config = ServerConfig {
        port: 7897,
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

/// 构造 oneshot 请求；可选 (body, content-type) 会附带 Content-Type 与
/// Content-Length 头。
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

/// 读取响应体并按 JSON 解析；任何失败都直接 panic（测试断言用）。
async fn body_json(response: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("json body")
}

/// 读取响应体的原始字节（二进制图标内容断言用）。
async fn body_bytes(response: axum::response::Response) -> Vec<u8> {
    axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body")
        .to_vec()
}

// HTTP fetch fakes (models.dev + npm).

/// 构造一个 fake HTTP 响应（models.dev 与 npm 的 fetch 桩共用）。
fn ok_response(status: u16, headers: Vec<(&str, &str)>, body: &str) -> HttpResponse {
    HttpResponse {
        status,
        headers: headers
            .into_iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect(),
        body: body.as_bytes().to_vec(),
    }
}

/// 记录型 fake fetch：每次调用记录请求并计数，固定返回给定响应；
/// 用于断言请求头与请求次数。
fn recording_fetch(
    response: HttpResponse,
) -> (HttpFetch, Arc<Mutex<Vec<HttpRequest>>>, Arc<AtomicUsize>) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let calls = Arc::new(AtomicUsize::new(0));
    let fetch: HttpFetch = {
        let requests = Arc::clone(&requests);
        let calls = Arc::clone(&calls);
        Arc::new(move |request: HttpRequest| {
            requests
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(request.clone());
            calls.fetch_add(1, Ordering::SeqCst);
            let response = response.clone();
            Box::pin(async move { Ok(response) }) as BoxFuture<_>
        })
    };
    (fetch, requests, calls)
}

/// 基于移植 fake 的确定性包管理器运行时：清空检测缓存，并把 OMPCHAMBER_*
/// 环境旋钮强制置空，保证测试不受宿主机环境影响。
/// Deterministic package-manager runtime over the ported fakes, with the
/// OMPCHAMBER_* env knobs forced unset.
fn fake_package_manager(
    transport: Arc<crate::package_manager::http::testing::FakeTransport>,
) -> Arc<PackageManagerRuntime> {
    crate::package_manager::detect::clear_detection_cache();
    let runner = crate::package_manager::spawn::testing::FakeRunner::new(|_, _| None);
    let mut env = HashMap::new();
    for key in [
        "OMPCHAMBER_UPDATE_API_URL",
        "OMPCHAMBER_RUNTIME",
        "OMPCHAMBER_PACKAGE_MANAGER",
        "OMPCHAMBER_SYSTEMD_UNIT",
        "INVOCATION_ID",
    ] {
        env.insert(key.to_string(), None);
    }
    Arc::new(PackageManagerRuntime::with_seams(runner, transport).with_env_overrides(env))
}
/// 绝不允许被调用的 proxy GET 桩：被调用即返回错误，用于断言“不该走
/// proxy”的路径。
fn never_proxy_get() -> super::models_metadata::ProxyGet {
    Arc::new(|_, _, _, _| {
        Box::pin(async move { Err("must not be called".to_string()) }) as BoxFuture<_>
    })
}

/// 恒返回空列表的 proxy 候选桩：模拟“未配置任何 proxy”的环境。
fn empty_candidates() -> super::models_metadata::ProxyCandidates {
    Arc::new(|| Box::pin(async move { Vec::new() }) as BoxFuture<_>)
}

/// 恒失败的 fake fetch：每次请求都返回同一个错误（网络失败路径用）。
fn failing_fetch(error: HttpError) -> HttpFetch {
    Arc::new(move |_request: HttpRequest| {
        let error = error.clone();
        Box::pin(async move { Err(error) }) as BoxFuture<_>
    })
}

/// 组装测试用目录缓存：注入给定 fetch、禁用 proxy、使用真实系统时钟。
fn fake_catalog(fetch: HttpFetch, cache_path: PathBuf) -> Arc<ModelsMetadataCache> {
    Arc::new(ModelsMetadataCache::new(
        CatalogIo {
            fetch,
            proxy_get: never_proxy_get(),
            proxy_candidates: empty_candidates(),
        },
        cache_path,
        Arc::new(models_metadata::system_now_ms),
    ))
}

/// 直接向磁盘写入一份合法的目录缓存文件（预热/老化测试的种子数据）。
fn write_disk_cache(cache_path: &Path, etag: &str, fetched_at: u64, data: Value) {
    std::fs::write(
        cache_path,
        serde_json::to_string(&json!({
            "version": 1,
            "etag": etag,
            "fetchedAt": fetched_at,
            "data": data,
        }))
        .unwrap(),
    )
    .unwrap();
}

// ---------------------------------------------------------------------------
// models-metadata.js
// ---------------------------------------------------------------------------

/// 验证：网络彻底失败时，过期的磁盘缓存以 stale 身份兜底返回。
#[tokio::test]
async fn disk_cache_serves_stale_when_network_fails() {
    let dir = temp_dir_unique("models-stale");
    let cache_path = dir.join("models-dev.catalog.json");
    write_disk_cache(
        &cache_path,
        "\"e1\"",
        system_now_minus(60_000_000),
        json!({ "anthropic": {} }),
    );

    let cache = fake_catalog(
        failing_fetch(HttpError::Other("fetch failed".into())),
        cache_path,
    );
    let result = cache
        .get(
            models_metadata::MODELS_DEV_API_URL,
            models_metadata::DEFAULT_TTL_MS,
            models_metadata::DEFAULT_TIMEOUT_MS,
        )
        .await
        .expect("stale fallback");
    assert!(result.from_cache);
    assert!(result.stale);
    assert_eq!(result.metadata, json!({ "anthropic": {} }));
}

/// 验证：磁盘缓存仍在 TTL 内时直接命中，完全不发起网络请求。
#[tokio::test]
async fn fresh_disk_cache_is_served_without_touching_the_network() {
    let dir = temp_dir_unique("models-fresh");
    let cache_path = dir.join("models-dev.catalog.json");
    write_disk_cache(
        &cache_path,
        "\"e1\"",
        models_metadata::system_now_ms(),
        json!({ "anthropic": {} }),
    );

    let (fetch, _requests, calls) =
        recording_fetch(ok_response(500, Vec::new(), "should not fetch"));
    let cache = fake_catalog(fetch, cache_path);
    let result = cache
        .get(
            models_metadata::MODELS_DEV_API_URL,
            models_metadata::DEFAULT_TTL_MS,
            models_metadata::DEFAULT_TIMEOUT_MS,
        )
        .await
        .expect("fresh cache");
    assert!(result.from_cache);
    assert!(!result.stale);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

/// 验证：成功刷新后新 ETag 与新正文被写回磁盘缓存文件。
#[tokio::test]
async fn refresh_persists_etag_and_payload_back_to_disk() {
    let dir = temp_dir_unique("models-persist");
    let cache_path = dir.join("models-dev.catalog.json");
    write_disk_cache(
        &cache_path,
        "\"e1\"",
        system_now_minus(60_000_000),
        json!({ "anthropic": {} }),
    );

    let (fetch, _requests, _calls) = recording_fetch(ok_response(
        200,
        vec![("etag", "\"e2\"")],
        r#"{"openai": {"models": {}}}"#,
    ));
    let cache = fake_catalog(fetch, cache_path.clone());
    let result = cache
        .get(
            models_metadata::MODELS_DEV_API_URL,
            models_metadata::DEFAULT_TTL_MS,
            models_metadata::DEFAULT_TIMEOUT_MS,
        )
        .await
        .expect("refresh");
    assert!(!result.from_cache);
    assert_eq!(result.metadata, json!({ "openai": { "models": {} } }));

    let disk: Value = serde_json::from_str(&std::fs::read_to_string(&cache_path).unwrap()).unwrap();
    assert_eq!(disk.get("etag"), Some(&json!("\"e2\"")));
    assert_eq!(
        disk.get("data"),
        Some(&json!({ "openai": { "models": {} } }))
    );
}

/// 验证：刷新时携带 If-None-Match；304 保留原正文并推进时间戳（随即视为新鲜）。
#[tokio::test]
async fn etag_revalidation_sends_if_none_match_and_keeps_body_on_304() {
    let dir = temp_dir_unique("models-304");
    let cache_path = dir.join("models-dev.catalog.json");
    write_disk_cache(
        &cache_path,
        "\"e1\"",
        system_now_minus(60_000_000),
        json!({ "cached": true }),
    );

    let (fetch, requests, _calls) = recording_fetch(ok_response(304, Vec::new(), ""));
    let cache = fake_catalog(fetch, cache_path.clone());
    let result = cache
        .get(
            models_metadata::MODELS_DEV_API_URL,
            models_metadata::DEFAULT_TTL_MS,
            models_metadata::DEFAULT_TIMEOUT_MS,
        )
        .await
        .expect("revalidated");
    let requests = requests.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(requests.len(), 1);
    let if_none_match = requests[0]
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("If-None-Match"))
        .map(|(_, value)| value.clone())
        .expect("If-None-Match sent");
    assert_eq!(if_none_match, "\"e1\"");
    assert_eq!(result.metadata, json!({ "cached": true }));
    assert!(!result.from_cache, "revalidated, not stale fallback");

    // fetchedAt moved to now → immediately fresh without another request.
    let (fetch, _requests, calls) = recording_fetch(ok_response(500, Vec::new(), "unreached"));
    let cache = fake_catalog(fetch, cache_path);
    let again = cache
        .get(
            models_metadata::MODELS_DEV_API_URL,
            models_metadata::DEFAULT_TTL_MS,
            models_metadata::DEFAULT_TIMEOUT_MS,
        )
        .await
        .expect("fresh after revalidation");
    assert!(again.from_cache);
    assert!(!again.stale);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

/// 验证：源站 503 属真实回答，不触发 proxy 重试（proxy 与候选探测零调用），最终回退 stale 缓存。
#[tokio::test]
async fn origin_http_errors_do_not_trigger_the_proxy_retry() {
    let dir = temp_dir_unique("models-503");
    let cache_path = dir.join("models-dev.catalog.json");
    write_disk_cache(
        &cache_path,
        "\"e1\"",
        system_now_minus(60_000_000),
        json!({ "cached": true }),
    );

    let proxy_calls = Arc::new(AtomicUsize::new(0));
    let proxy_get: super::models_metadata::ProxyGet = {
        let proxy_calls = Arc::clone(&proxy_calls);
        Arc::new(move |_, _, _, _| {
            proxy_calls.fetch_add(100, Ordering::SeqCst);
            Box::pin(async move { Err("must not be called".to_string()) }) as BoxFuture<_>
        })
    };
    let candidate_calls = Arc::new(AtomicUsize::new(0));
    let proxy_candidates: super::models_metadata::ProxyCandidates = {
        let candidate_calls = Arc::clone(&candidate_calls);
        Arc::new(move || {
            candidate_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                vec![ProxyEndpoint {
                    host: "p".into(),
                    port: 1,
                }]
            }) as BoxFuture<_>
        })
    };
    let cache = Arc::new(ModelsMetadataCache::new(
        CatalogIo {
            fetch: recording_fetch(ok_response(503, Vec::new(), "nope")).0,
            proxy_get,
            proxy_candidates,
        },
        cache_path,
        Arc::new(models_metadata::system_now_ms),
    ));

    let result = cache
        .get(
            models_metadata::MODELS_DEV_API_URL,
            models_metadata::DEFAULT_TTL_MS,
            models_metadata::DEFAULT_TIMEOUT_MS,
        )
        .await
        .expect("stale fallback after origin 503");
    assert!(result.from_cache && result.stale);
    assert_eq!(result.metadata, json!({ "cached": true }));
    assert_eq!(proxy_calls.load(Ordering::SeqCst), 0, "no proxy attempt");
    assert_eq!(
        candidate_calls.load(Ordering::SeqCst),
        0,
        "no candidate detection"
    );
}

/// 验证：直连网络级失败后按候选顺序经 proxy 重试并采用其响应。
#[tokio::test]
async fn direct_network_failure_retries_through_the_proxy() {
    let dir = temp_dir_unique("models-proxy");
    let cache_path = dir.join("models-dev.catalog.json");
    write_disk_cache(
        &cache_path,
        "\"e1\"",
        system_now_minus(60_000_000),
        json!({ "anthropic": {} }),
    );

    let proxy_get: super::models_metadata::ProxyGet = Arc::new(|_, _, _, _| {
        Box::pin(async move {
            Ok(super::models_metadata::ProxyResponse {
                status: 200,
                etag: Some("\"p1\"".into()),
                body: r#"{"via": "proxy"}"#.into(),
            })
        })
    });
    let proxy_candidates: super::models_metadata::ProxyCandidates = Arc::new(|| {
        Box::pin(async move {
            vec![ProxyEndpoint {
                host: "127.0.0.1".into(),
                port: 9,
            }]
        })
    });
    let cache = Arc::new(ModelsMetadataCache::new(
        CatalogIo {
            fetch: failing_fetch(HttpError::Other("fetch failed: socket hang up".into())),
            proxy_get,
            proxy_candidates,
        },
        cache_path,
        Arc::new(models_metadata::system_now_ms),
    ));

    let result = cache
        .get(
            models_metadata::MODELS_DEV_API_URL,
            models_metadata::DEFAULT_TTL_MS,
            models_metadata::DEFAULT_TIMEOUT_MS,
        )
        .await
        .expect("via proxy");
    assert!(!result.from_cache);
    assert_eq!(result.metadata, json!({ "via": "proxy" }));
}

/// 验证：网络与 proxy 全部失败且无任何缓存时，向上返回错误。
#[tokio::test]
async fn total_failure_without_any_cache_is_an_error() {
    let dir = temp_dir_unique("models-error");
    let cache = fake_catalog(
        failing_fetch(HttpError::Other("fetch failed".into())),
        dir.join("models-dev.catalog.json"),
    );
    let error = cache
        .get(
            models_metadata::MODELS_DEV_API_URL,
            models_metadata::DEFAULT_TTL_MS,
            models_metadata::DEFAULT_TIMEOUT_MS,
        )
        .await
        .expect_err("nothing cached");
    assert_eq!(error.message, "no proxy configured");
}

/// 验证：env proxy 解析只接受 http/https（socks 与非法 URL 跳过），且 env 列表本身不去重。
#[test]
fn env_proxy_candidates_parse_and_dedupe_http_proxies_only() {
    let candidates = env_proxies_from(&[
        Some("http://127.0.0.1:7890".into()),
        Some("socks5://127.0.0.1:1080".into()), // socks → skipped
        Some("https://proxy.corp".into()),      // default https port
        Some("not a url".into()),               // unparseable → skipped
        None,
    ]);
    assert_eq!(
        candidates,
        vec![
            ProxyEndpoint {
                host: "127.0.0.1".into(),
                port: 7890
            },
            ProxyEndpoint {
                host: "proxy.corp".into(),
                port: 443
            },
        ]
    );
    // Deduplication happens in `detectProxyCandidates`, after the env and
    // system lists are concatenated — not in the env list itself.
    assert_eq!(
        env_proxies_from(&[Some("http://a:1".into()), Some("http://a:1".into())]).len(),
        2
    );
}

/// 验证：scutil 输出解析只提取已启用（Enable=1）的代理键值对。
#[test]
fn scutil_output_parsing_reads_enabled_proxies() {
    let stdout = "\
<dictionary> {
  HTTPSEnable : 1
  HTTPSProxy : 127.0.0.1
  HTTPSPort : 7890
  HTTPEnable : 0
  HTTPProxy : 127.0.0.1
  HTTPPort : 8080
}
";
    assert_eq!(
        parse_scutil_output(stdout),
        vec![ProxyEndpoint {
            host: "127.0.0.1".into(),
            port: 7890
        }]
    );
    assert!(parse_scutil_output("scutil: no proxies").is_empty());
}

/// 当前时间减去给定毫秒（饱和减法），用于把缓存种子“老化”到过期。
fn system_now_minus(delta: u64) -> u64 {
    models_metadata::system_now_ms().saturating_sub(delta)
}

// ---------------------------------------------------------------------------
// npm-registry.js
// ---------------------------------------------------------------------------

/// npm 测试串行化：它们共享进程级 CACHE/INFLIGHT 静态量，并行测试的
/// `clear_cache()` 会在查找进行中清掉 per-name mutex 与缓存。
/// Serializes the npm tests: they share the process-wide CACHE/INFLIGHT
/// statics, and a parallel test's `clear_cache()` during an in-flight
/// lookup would wipe the per-name mutex and the cache mid-test.
async fn npm_test_serial() -> tokio::sync::MutexGuard<'static, ()> {
    // 所有 npm 测试共享的进程级串行锁。
    static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    SERIAL.lock().await
}

/// 由同步 handler 构造记录型 `NpmIo`（fake fetch + 请求日志），registry
/// 指向官方地址。
fn npm_io_with(
    handler: impl Fn(HttpRequest) -> Result<HttpResponse, HttpError> + Send + Sync + 'static,
) -> (NpmIo, Arc<Mutex<Vec<HttpRequest>>>) {
    npm_io_from(move |request| {
        let result = handler(request.clone());
        Box::pin(async move { result }) as BoxFuture<_>
    })
}

/// 同 [`npm_io_with`]，但 handler 返回 future，可延迟应答（in-flight 去重测试用）。
/// Same as [`npm_io_with`] for handlers that need to await before answering
/// (in-flight dedup tests).
fn npm_io_with_future(
    handler: impl Fn(HttpRequest) -> BoxFuture<'static, Result<HttpResponse, HttpError>>
    + Send
    + Sync
    + 'static,
) -> (NpmIo, Arc<Mutex<Vec<HttpRequest>>>) {
    npm_io_from(handler)
}

/// 记录型 `NpmIo` 的共同实现：把 handler 包装为记录请求的 `HttpFetch`，
/// 并共享请求日志供断言。
fn npm_io_from(
    handler: impl Fn(HttpRequest) -> BoxFuture<'static, Result<HttpResponse, HttpError>>
    + Send
    + Sync
    + 'static,
) -> (NpmIo, Arc<Mutex<Vec<HttpRequest>>>) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let fetch: HttpFetch = {
        let requests = Arc::clone(&requests);
        Arc::new(move |request: HttpRequest| {
            let future = handler(request.clone());
            requests
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(request);
            future
        })
    };
    (
        NpmIo {
            fetch,
            registry_base: "https://registry.npmjs.org".to_string(),
        },
        requests,
    )
}

/// 验证：npm 查询成功时返回 latest、全部版本号与 dist-tags 映射。
#[tokio::test]
async fn npm_lookup_success_returns_latest_versions_and_dist_tags() {
    let _npm_serial = npm_test_serial().await;
    npm_registry::clear_cache();
    let (io, _requests) = npm_io_with(|_| {
        Ok(ok_response(
            200,
            vec![("content-type", "application/json")],
            r#"{"dist-tags": {"latest": "1.2.0", "next": "2.0.0-rc.1"}, "versions": {"1.0.0": {}, "1.2.0": {}}}"#,
        ))
    });
    let result = npm_registry::lookup_npm_package(&io, "foo").await;
    assert_eq!(
        result.to_value(),
        json!({
            "ok": true,
            "latest": "1.2.0",
            "versions": ["1.0.0", "1.2.0"],
            "distTags": { "latest": "1.2.0", "next": "2.0.0-rc.1" },
        })
    );
}

/// 验证：dist-tags/versions 缺失时以 null/空数组等 JS 兼容形状返回 ok。
#[tokio::test]
async fn npm_lookup_handles_missing_dist_tags_and_versions() {
    let _npm_serial = npm_test_serial().await;
    npm_registry::clear_cache();
    let (io, _requests) = npm_io_with(|_| Ok(ok_response(200, Vec::new(), "{}")));
    let result = npm_registry::lookup_npm_package(&io, "foo").await;
    assert_eq!(
        result.to_value(),
        json!({ "ok": true, "latest": null, "versions": [], "distTags": {} })
    );
}

/// 验证：registry 404 映射为 "Package not found" 错误。
#[tokio::test]
async fn npm_lookup_404_returns_package_not_found() {
    let _npm_serial = npm_test_serial().await;
    npm_registry::clear_cache();
    let (io, _requests) = npm_io_with(|_| Ok(ok_response(404, Vec::new(), "{}")));
    let result = npm_registry::lookup_npm_package(&io, "missing").await;
    assert_eq!(
        result,
        NpmInfo::Err {
            status: NpmStatus::Code(404),
            error: "Package not found".to_string(),
        }
    );
}

/// 验证：registry 500 映射为 "Registry returned 500" 错误。
#[tokio::test]
async fn npm_lookup_500_returns_registry_error() {
    let _npm_serial = npm_test_serial().await;
    npm_registry::clear_cache();
    let (io, _requests) = npm_io_with(|_| Ok(ok_response(500, Vec::new(), "{}")));
    let result = npm_registry::lookup_npm_package(&io, "foo").await;
    assert_eq!(
        result,
        NpmInfo::Err {
            status: NpmStatus::Code(500),
            error: "Registry returned 500".to_string(),
        }
    );
}

/// 验证：网络层错误返回 status=Network 的错误形状。
#[tokio::test]
async fn npm_lookup_network_error_returns_network_status() {
    let _npm_serial = npm_test_serial().await;
    npm_registry::clear_cache();
    let (io, _requests) = npm_io_with(|_| Err(HttpError::Other("socket closed".into())));
    let result = npm_registry::lookup_npm_package(&io, "foo").await;
    assert_eq!(
        result,
        NpmInfo::Err {
            status: NpmStatus::Network,
            error: "socket closed".to_string(),
        }
    );
}

/// 验证：scoped 包名中的斜杠被编码为 %2F 拼入 registry URL。
#[tokio::test]
async fn npm_scoped_names_encode_the_slash_in_the_registry_url() {
    let _npm_serial = npm_test_serial().await;
    npm_registry::clear_cache();
    let (io, requests) = npm_io_with(|_| Ok(ok_response(200, Vec::new(), "{}")));
    npm_registry::get_npm_info(&io, "@scope/pkg", false).await;
    let requests = requests.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(requests[0].url, "https://registry.npmjs.org/@scope%2Fpkg");
}

/// 验证：registry 请求携带 ompchamber-server/ User-Agent 与 Accept 头。
#[tokio::test]
async fn npm_user_agent_and_accept_headers_are_present() {
    let _npm_serial = npm_test_serial().await;
    npm_registry::clear_cache();
    let (io, requests) = npm_io_with(|_| Ok(ok_response(200, Vec::new(), "{}")));
    npm_registry::get_npm_info(&io, "foo", false).await;
    let requests = requests.lock().unwrap_or_else(|e| e.into_inner());
    let user_agent = requests[0]
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("User-Agent"))
        .map(|(_, value)| value.clone())
        .expect("User-Agent");
    assert!(user_agent.starts_with("ompchamber-server/"));
    assert!(
        requests[0]
            .headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("Accept"))
    );
}

/// 验证：缓存命中时两次查询返回同一结果（不再发请求）。
#[tokio::test]
async fn npm_cache_hit_reuses_definitive_success() {
    let _npm_serial = npm_test_serial().await;
    npm_registry::clear_cache();
    let (io, _requests) = npm_io_with(|_| {
        Ok(ok_response(
            200,
            Vec::new(),
            r#"{"dist-tags": {"latest": "1.0.0"}, "versions": {"1.0.0": {}}}"#,
        ))
    });
    let first = npm_registry::get_npm_info(&io, "foo", false).await;
    let second = npm_registry::get_npm_info(&io, "foo", false).await;
    assert_eq!(first, second);
}

/// 验证：缓存条目超过 1 小时 TTL 后过期，触发第二次网络请求。
#[tokio::test]
async fn npm_cache_miss_after_ttl_fetches_again() {
    let _npm_serial = npm_test_serial().await;
    npm_registry::clear_cache();
    let (io, requests) = npm_io_with(|_| Ok(ok_response(200, Vec::new(), r#"{"versions": {}}"#)));
    npm_registry::get_npm_info(&io, "ttl-pkg", false).await;
    // Age the entry past the 1h TTL.
    npm_registry::testing::set_cache_entry(
        "ttl-pkg",
        models_metadata::system_now_ms().saturating_sub(3_600_001),
        NpmInfo::Ok {
            latest: None,
            versions: Vec::new(),
            dist_tags: serde_json::Map::new(),
        },
    );
    npm_registry::get_npm_info(&io, "ttl-pkg", false).await;
    let requests = requests.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(requests.len(), 2);
}

/// 验证：force 刷新绕过缓存直接发起网络请求。
#[tokio::test]
async fn npm_force_refresh_bypasses_the_cache() {
    let _npm_serial = npm_test_serial().await;
    npm_registry::clear_cache();
    let (io, requests) = npm_io_with(|_| Ok(ok_response(200, Vec::new(), r#"{"versions": {}}"#)));
    npm_registry::get_npm_info(&io, "force-pkg", false).await;
    npm_registry::get_npm_info(&io, "force-pkg", true).await;
    let requests = requests.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(requests.len(), 2);
}

/// 验证：同名包的并发查询合并为一次 registry 请求（per-name 串行 + 缓存复读）。
#[tokio::test]
async fn npm_in_flight_requests_dedup_by_package_name() {
    let _npm_serial = npm_test_serial().await;
    npm_registry::clear_cache();
    // The JS shares one in-flight promise; the Rust port serializes on a
    // per-name mutex and then re-reads the cache (documented divergence —
    // same wire result, one registry fetch either way). The delayed
    // response keeps the first caller in flight while the second parks on
    // the per-name mutex.
    let (io, requests) = npm_io_with_future(|_| {
        Box::pin(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            Ok(ok_response(
                200,
                Vec::new(),
                r#"{"versions": {"1.0.0": {}}}"#,
            ))
        }) as BoxFuture<_>
    });
    let first = tokio::spawn({
        let io = fake_npm_io_handle(&io);
        async move { npm_registry::get_npm_info(&io, "dedup", false).await }
    });
    let second = tokio::spawn({
        let io = fake_npm_io_handle(&io);
        async move { npm_registry::get_npm_info(&io, "dedup", false).await }
    });
    let (first, second) = tokio::join!(first, second);
    let first = first.expect("first task");
    let second = second.expect("second task");
    assert!(matches!(first, NpmInfo::Ok { .. }));
    assert!(matches!(second, NpmInfo::Ok { .. }));
    let requests = requests.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(requests.len(), 1, "one registry fetch");
}

/// 克隆 `NpmIo` 句柄（fetch 闭包共享），供移动进 spawn 的任务使用。
/// Clone handle for moving the io into a spawned task.
fn fake_npm_io_handle(io: &NpmIo) -> NpmIo {
    NpmIo {
        fetch: Arc::clone(&io.fetch),
        registry_base: io.registry_base.clone(),
    }
}

/// 验证：网络错误不进缓存（下次重新请求），而 404 作为确定性结果被缓存。
#[tokio::test]
async fn npm_network_failure_is_not_cached_but_404_is() {
    let _npm_serial = npm_test_serial().await;
    npm_registry::clear_cache();
    let (io, requests) = npm_io_with(|_| {
        use std::sync::atomic::AtomicU8;
        // 计数器：第一次调用模拟网络错误，之后返回成功。
        static CALL: AtomicU8 = AtomicU8::new(0);

        if CALL.fetch_add(1, Ordering::SeqCst) == 0 {
            Err(HttpError::Other("down".into()))
        } else {
            Ok(ok_response(200, Vec::new(), r#"{"versions": {}}"#))
        }
    });
    let first = npm_registry::get_npm_info(&io, "flaky", false).await;
    let second = npm_registry::get_npm_info(&io, "flaky", false).await;
    assert!(matches!(
        first,
        NpmInfo::Err {
            status: NpmStatus::Network,
            ..
        }
    ));
    assert!(matches!(second, NpmInfo::Ok { .. }));
    let requests = requests.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(requests.len(), 2);

    let (io, requests) = npm_io_with(|_| Ok(ok_response(404, Vec::new(), "{}")));
    npm_registry::get_npm_info(&io, "gone", false).await;
    npm_registry::get_npm_info(&io, "gone", false).await;
    let requests = requests.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(requests.len(), 1, "404 is cached");
}

/// 验证：包名编码与 JS 的 encodeURIComponent 行为一致（@ 保留、斜杠转 %2F、空格转 %20）。
#[test]
fn npm_encode_name_matches_uri_component_plus_at_restore() {
    assert_eq!(npm_registry::encode_name("foo"), "foo");
    assert_eq!(npm_registry::encode_name("@scope/pkg"), "@scope%2Fpkg");
    assert_eq!(npm_registry::encode_name("a b"), "a%20b");
}

// ---------------------------------------------------------------------------
// openchamber-routes.js — pure helpers
// ---------------------------------------------------------------------------

/// 验证：UA 嗅探正确区分 unknown/tablet/mobile/desktop。
#[test]
fn infer_device_class_sniffs_the_user_agent() {
    assert_eq!(infer_device_class(""), "unknown");
    assert_eq!(
        infer_device_class("Mozilla/5.0 (iPad; CPU OS 16_0)"),
        "tablet"
    );
    assert_eq!(infer_device_class("Android 14 Mobile Safari"), "mobile");
    assert_eq!(infer_device_class("iPhone like Geo"), "mobile");
    assert_eq!(infer_device_class("Mozilla/5.0 Macintosh"), "desktop");
}

/// 验证：reportUsage 只有字面 false/0/no 退出，缺省与其它值视为同意上报。
#[test]
fn parse_report_usage_opts_out_only_on_literal_false_spellings() {
    assert_eq!(parse_report_usage(None), None);
    assert_eq!(parse_report_usage(Some("false")), Some(false));
    assert_eq!(parse_report_usage(Some("0")), Some(false));
    assert_eq!(parse_report_usage(Some("NO")), Some(false));
    assert_eq!(parse_report_usage(Some("yes")), Some(true));
    assert_eq!(parse_report_usage(Some("")), Some(true));
}

/// 验证：systemd unit 解析要求 INVOCATION_ID 存在、默认 unit 兜底、拒绝不安全名。
#[test]
fn systemd_unit_resolution_requires_invocation_id_and_a_safe_unit() {
    assert_eq!(
        resolve_systemd_service_unit(None, Some("ompchamber.service")),
        None
    );
    assert_eq!(
        resolve_systemd_service_unit(Some(""), Some("ompchamber.service")),
        None
    );
    // Default unit when no override is configured.
    assert_eq!(
        resolve_systemd_service_unit(Some("invocation"), None),
        Some("ompchamber.service".to_string())
    );
    assert_eq!(
        resolve_systemd_service_unit(Some("invocation"), Some("  ompchamber@wsl.service  ")),
        Some("ompchamber@wsl.service".to_string())
    );
    // Unsafe overrides are rejected before any job is queued.
    assert_eq!(
        resolve_systemd_service_unit(Some("invocation"), Some("ompchamber.service; rm -rf /")),
        None
    );
    assert_eq!(
        resolve_systemd_service_unit(Some("invocation"), Some("a/b.service")),
        None
    );
    assert_eq!(
        resolve_systemd_service_unit(Some("invocation"), Some(".service")),
        None
    );
}

/// 验证：安装决策树的四个分支（容器优先、前台±systemd、daemon）。
#[test]
fn install_decision_walks_the_js_tree() {
    use InstallDecision::*;
    assert_eq!(install_decision(true, true, None), Container);
    assert_eq!(
        install_decision(false, true, None),
        ForegroundWithoutSystemd
    );
    assert_eq!(
        install_decision(false, true, Some("ompchamber.service".into())),
        ForegroundSystemd("ompchamber.service".into())
    );
    assert_eq!(
        install_decision(false, false, Some("ompchamber.service".into())),
        DaemonRestart
    );
}

// ---------------------------------------------------------------------------
// project-icon-routes.js — pure helpers
// ---------------------------------------------------------------------------

/// 验证：SVG 主题注入把默认主题色写进紧跟 <svg> 开标签的 <style>。
#[test]
fn svg_theme_injects_the_default_color_after_the_svg_open_tag() {
    let markup = r#"<?xml version="1.0"?><svg xmlns="a"><path d="M0 0"/></svg>"#;
    let themed = apply_project_icon_svg_theme(markup, Some("dark"), None);
    assert!(themed.contains(
        r#"><style data-ompchamber-theme-icon="1">:root{color:#f5f5f5!important;}</style><path"#
    ));
    let themed = apply_project_icon_svg_theme(markup, Some("light"), None);
    assert!(themed.contains("color:#111111!important"));
}

/// 验证：显式颜色优先于主题色；无主题不动原文；<svgx 不误匹配。
#[test]
fn svg_theme_explicit_color_wins_and_no_theme_leaves_markup_alone() {
    let markup = "<SVG viewBox='0 0 1 1'><g/></SVG>";
    let themed = apply_project_icon_svg_theme(markup, Some("dark"), Some("#ABCDEF"));
    assert!(themed.contains("color:#ABCDEF!important"));
    assert_eq!(apply_project_icon_svg_theme(markup, None, None), markup);
    // `<svgx` must not match (`\b` after the tag name).
    assert_eq!(
        apply_project_icon_svg_theme("<svgx><g/></svgx>", Some("dark"), None),
        "<svgx><g/></svgx>"
    );
    // No closing `>` → left untouched.
    assert_eq!(
        apply_project_icon_svg_theme("<svg", Some("dark"), None),
        "<svg"
    );
}

/// 验证：查询串解析的 first/single 语义与 Express 版一致。
#[test]
fn query_parsing_first_and_single_semantics_match_express() {
    let query = parse_query(Some("theme=dark&theme=light&iconColor=%23abc&plain"));
    assert_eq!(query.first("theme"), Some("dark"));
    assert_eq!(query.single("theme"), None, "repeated keys are arrays");
    assert_eq!(query.single("iconColor"), Some("#abc"));
    assert_eq!(query.single("plain"), Some(""));
    assert_eq!(query.first("missing"), None);
    assert!(parse_query(None).first("any").is_none());
}

// ---------------------------------------------------------------------------
// project-icon-routes.js — routes
// ---------------------------------------------------------------------------

/// 计算字符串的 SHA-1 hex 摘要（复用 walkthrough 的 Node 兼容实现），
/// 用于拼出图标文件名。
fn sha1_hex(value: &str) -> String {
    crate::walkthrough::sha1::sha1_hex(value.as_bytes())
}

/// 在数据目录种下一个项目：创建目录、生成确定性 id
/// （path_<base64url(规范化路径)>）并把项目写进 settings.json，返回 id。
fn seed_project(data_dir: &Path, project_path: &Path, icon_image: Value) -> String {
    std::fs::create_dir_all(project_path).unwrap();
    let canonical = std::fs::canonicalize(project_path).unwrap();
    // Deterministic ids (`path_<base64url(canonical)>`) survive the settings
    // migrations untouched, like the JS's post-migration state.
    let id = format!("path_{}", {
        use base64::Engine as _;
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(canonical.to_string_lossy().as_bytes())
    });
    std::fs::write(
        data_dir.join("settings.json"),
        serde_json::to_string(&json!({
            "projects": [{
                "id": id,
                "path": canonical.to_string_lossy(),
                "iconImage": icon_image,
            }],
        }))
        .unwrap(),
    )
    .unwrap();
    id
}

/// 把字节编码为 `data:image/png;base64,...` 的 data URL（PUT 请求体用）。
fn png_data_url(bytes: &[u8]) -> String {
    use base64::engine::Engine as _;
    format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
}

/// 挂载完整 meta 路由（图标测试用）：目录缓存指向必然失败的 fetch，
/// 包管理器走空 fake transport。
fn icon_app_router(data_dir: &Path) -> axum::Router {
    super::router_with(
        test_context(data_dir),
        fake_catalog(
            failing_fetch(HttpError::Other("no network in icon tests".into())),
            data_dir.join("models-dev.catalog.json"),
        ),
        fake_package_manager(crate::package_manager::http::testing::FakeTransport::new()),
    )
}

/// 验证：元数据指向的图标缺失时按扩展名候选回退，MIME 由扩展名推导。
#[tokio::test]
async fn get_icon_uses_fallback_extension_mime_when_metadata_points_to_a_missing_icon() {
    let data_dir = temp_dir_unique("icon-fallback");
    let project_dir = data_dir.join("repo");
    let id = seed_project(
        &data_dir,
        &project_dir,
        json!({ "mime": "image/png", "updatedAt": 1, "source": "custom" }),
    );

    let icons_dir = data_dir.join("project-icons");
    std::fs::create_dir_all(&icons_dir).unwrap();
    let jpg_bytes = b"jpg-bytes".to_vec();
    std::fs::write(
        icons_dir.join(format!("project-{}.jpg", sha1_hex(&id))),
        &jpg_bytes,
    )
    .unwrap();

    let app = icon_app_router(&data_dir);
    let response = app
        .oneshot(request_json(
            "GET",
            &format!("/api/projects/{id}/icon"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        "image/jpeg"
    );
    assert_eq!(
        response.headers().get(header::CACHE_CONTROL).unwrap(),
        "public, max-age=31536000, immutable"
    );
    assert_eq!(body_bytes(response).await, jpg_bytes);
}

/// 验证：GET 图标支持 ?theme= 对 SVG 注入主题色并带正确的 Content-Type。
#[tokio::test]
async fn get_icon_serves_themed_svg_with_query_color() {
    let data_dir = temp_dir_unique("icon-svg");
    let project_dir = data_dir.join("repo");
    let id = seed_project(
        &data_dir,
        &project_dir,
        json!({ "mime": "image/svg+xml", "updatedAt": 1, "source": "custom" }),
    );

    let icons_dir = data_dir.join("project-icons");
    std::fs::create_dir_all(&icons_dir).unwrap();
    let svg = br#"<svg xmlns="a"><path/></svg>"#.to_vec();
    std::fs::write(
        icons_dir.join(format!("project-{}.svg", sha1_hex(&id))),
        &svg,
    )
    .unwrap();

    let app = icon_app_router(&data_dir);
    let response = app
        .oneshot(request_json(
            "GET",
            &format!("/api/projects/{id}/icon?theme=dark"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        "image/svg+xml; charset=utf-8"
    );
    let body = String::from_utf8(body_bytes(response).await).unwrap();
    assert!(
        body.contains("color:#f5f5f5!important"),
        "dark theme color: {body}"
    );
}

/// 验证：projectId 为空 400、项目未知 404、无图标文件 404 的完整错误路径。
#[tokio::test]
async fn get_icon_requires_a_project_and_finds_the_icon() {
    let data_dir = temp_dir_unique("icon-404s");
    let project_dir = data_dir.join("repo");
    let id = seed_project(&data_dir, &project_dir, Value::Null);

    let app = icon_app_router(&data_dir);
    // Blank projectId (percent-encoded space) → 400.
    let response = app
        .clone()
        .oneshot(request_json("GET", "/api/projects/%20/icon", None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_json(response).await,
        json!({ "error": "projectId is required" })
    );

    // Unknown project → 404 Project not found.
    let response = app
        .clone()
        .oneshot(request_json("GET", "/api/projects/other/icon", None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        body_json(response).await,
        json!({ "error": "Project not found" })
    );

    // Known project without icon files → 404 Project icon not found.
    let response = app
        .oneshot(request_json(
            "GET",
            &format!("/api/projects/{id}/icon"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        body_json(response).await,
        json!({ "error": "Project icon not found" })
    );
}

/// 验证：PUT 落盘新图标、写回 iconImage 元数据并清理其它扩展名旧文件。
#[tokio::test]
async fn put_icon_uploads_persists_and_clears_other_extensions() {
    let data_dir = temp_dir_unique("icon-put");
    let project_dir = data_dir.join("repo");
    let id = seed_project(&data_dir, &project_dir, Value::Null);

    let icons_dir = data_dir.join("project-icons");
    std::fs::create_dir_all(&icons_dir).unwrap();
    std::fs::write(
        icons_dir.join(format!("project-{}.jpg", sha1_hex(&id))),
        b"old",
    )
    .unwrap();

    let app = icon_app_router(&data_dir);
    let png = b"png-bytes".to_vec();
    let response = app
        .oneshot(request_json(
            "PUT",
            &format!("/api/projects/{id}/icon"),
            Some((
                &serde_json::to_string(&json!({ "dataUrl": png_data_url(&png) })).unwrap(),
                "application/json",
            )),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let payload = body_json(response).await;
    let project = payload.get("project").unwrap();
    assert_eq!(
        project.get("iconImage").unwrap().get("mime"),
        Some(&json!("image/png"))
    );
    assert_eq!(
        project.get("iconImage").unwrap().get("source"),
        Some(&json!("custom"))
    );
    assert!(payload.get("settings").unwrap().get("projects").is_some());
    assert_eq!(
        std::fs::read(icons_dir.join(format!("project-{}.png", sha1_hex(&id)))).unwrap(),
        png
    );
    assert!(
        !icons_dir
            .join(format!("project-{}.jpg", sha1_hex(&id)))
            .exists(),
        "previous extension removed"
    );
}

/// 验证：dataUrl 校验的各失败分支返回与 JS 一致的 400 文案，校验通过后才查项目（404）。
#[tokio::test]
async fn put_icon_validates_the_data_url() {
    let data_dir = temp_dir_unique("icon-put-validation");
    let project_dir = data_dir.join("repo");
    let id = seed_project(&data_dir, &project_dir, Value::Null);

    let app = icon_app_router(&data_dir);
    let cases: Vec<(&str, &str)> = vec![
        (r#"{"dataUrl": "not-a-data-url"}"#, "Invalid dataUrl format"),
        (
            r#"{"dataUrl": "data:image/webp;base64,AAAA"}"#,
            "Icon must be PNG, JPEG, or SVG",
        ),
        (
            r#"{"dataUrl": "data:image/png;base64, }"}"#,
            "Invalid dataUrl format",
        ),
        (r#"{}"#, "dataUrl is required"),
        // Trailing whitespace is trimmed before the regex, so only a
        // padding-only payload decodes to zero bytes ("Icon content is
        // empty").
        (
            r#"{"dataUrl": "data:image/png;base64,="}"#,
            "Icon content is empty",
        ),
    ];
    for (body, expected) in cases {
        let response = app
            .clone()
            .oneshot(request_json(
                "PUT",
                &format!("/api/projects/{id}/icon"),
                Some((body, "application/json")),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "case {body}");
        assert_eq!(body_json(response).await, json!({ "error": expected }));
    }

    // Unknown project after validation → 404.
    let response = app
        .clone()
        .oneshot(request_json(
            "PUT",
            "/api/projects/unknown/icon",
            Some((
                &serde_json::to_string(&json!({ "dataUrl": png_data_url(b"x") })).unwrap(),
                "application/json",
            )),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        body_json(response).await,
        json!({ "error": "Project not found" })
    );

    // A non-JSON body reads as `undefined` → dataUrl required.
    let response = app
        .oneshot(request_json(
            "PUT",
            &format!("/api/projects/{id}/icon"),
            Some(("hello", "text/plain")),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_json(response).await,
        json!({ "error": "dataUrl is required" })
    );
}

/// 验证：DELETE 删除全部图标文件并把 iconImage 置空持久化。
#[tokio::test]
async fn delete_icon_removes_files_and_clears_metadata() {
    let data_dir = temp_dir_unique("icon-delete");
    let project_dir = data_dir.join("repo");
    let id = seed_project(
        &data_dir,
        &project_dir,
        json!({ "mime": "image/png", "updatedAt": 1, "source": "custom" }),
    );

    let icons_dir = data_dir.join("project-icons");
    std::fs::create_dir_all(&icons_dir).unwrap();
    let icon_path = icons_dir.join(format!("project-{}.png", sha1_hex(&id)));
    std::fs::write(&icon_path, b"png").unwrap();

    let app = icon_app_router(&data_dir);
    let response = app
        .oneshot(request_json(
            "DELETE",
            &format!("/api/projects/{id}/icon"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let payload = body_json(response).await;
    assert_eq!(
        payload.get("project").unwrap().get("iconImage"),
        Some(&Value::Null)
    );
    assert!(!icon_path.exists());
}

/// 验证：discover 收养路径最短的 favicon，复制为 source=auto 的图标。
#[tokio::test]
async fn discover_icon_adopts_the_shortest_favicon_path() {
    let data_dir = temp_dir_unique("icon-discover");
    let project_dir = data_dir.join("repo");
    let id = seed_project(&data_dir, &project_dir, Value::Null);

    std::fs::create_dir_all(project_dir.join("deep/nested")).unwrap();
    std::fs::write(project_dir.join("deep/nested/favicon.png"), b"nested").unwrap();
    let ico = b"ico-bytes".to_vec();
    std::fs::write(project_dir.join("favicon.ico"), &ico).unwrap();

    let app = icon_app_router(&data_dir);
    let response = app
        .oneshot(request_json(
            "POST",
            &format!("/api/projects/{id}/icon/discover"),
            Some(("{}", "application/json")),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let payload = body_json(response).await;
    let icon_image = payload.get("project").unwrap().get("iconImage").unwrap();
    assert_eq!(icon_image.get("mime"), Some(&json!("image/x-icon")));
    assert_eq!(icon_image.get("source"), Some(&json!("auto")));
    let discovered = payload.get("discoveredPath").unwrap().as_str().unwrap();
    assert!(
        discovered.ends_with("favicon.ico"),
        "shortest path wins: {discovered}"
    );
    assert_eq!(
        std::fs::read(data_dir.join(format!("project-icons/project-{}.ico", sha1_hex(&id))))
            .unwrap(),
        ico
    );
}

/// 验证：已有 custom 图标时 discover 跳过并返回 skipped=true。
#[tokio::test]
async fn discover_icon_skips_custom_icons_without_force() {
    let data_dir = temp_dir_unique("icon-skip");
    let project_dir = data_dir.join("repo");
    let id = seed_project(
        &data_dir,
        &project_dir,
        json!({ "mime": "image/png", "updatedAt": 1, "source": "custom" }),
    );
    std::fs::write(project_dir.join("favicon.ico"), b"ico").unwrap();

    let app = icon_app_router(&data_dir);
    let response = app
        .oneshot(request_json(
            "POST",
            &format!("/api/projects/{id}/icon/discover"),
            Some(("{}", "application/json")),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        body_json(response).await,
        json!({
            "project": {
                "id": id,
                "path": std::fs::canonicalize(&project_dir).unwrap().to_string_lossy(),
                "iconImage": { "mime": "image/png", "updatedAt": 1, "source": "custom" },
            },
            "skipped": true,
            "reason": "custom-icon-present",
        })
    );
}

/// 验证：force=true 时 discover 覆盖 custom 图标改为自动收养。
#[tokio::test]
async fn discover_icon_force_overrides_a_custom_icon() {
    let data_dir = temp_dir_unique("icon-force");
    let project_dir = data_dir.join("repo");
    let id = seed_project(
        &data_dir,
        &project_dir,
        json!({ "mime": "image/png", "updatedAt": 1, "source": "custom" }),
    );
    std::fs::write(project_dir.join("favicon.svg"), b"<svg/>").unwrap();

    let app = icon_app_router(&data_dir);
    let response = app
        .oneshot(request_json(
            "POST",
            &format!("/api/projects/{id}/icon/discover"),
            Some((r#"{ "force": true }"#, "application/json")),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let payload = body_json(response).await;
    assert_eq!(
        payload
            .get("project")
            .unwrap()
            .get("iconImage")
            .unwrap()
            .get("source"),
        Some(&json!("auto"))
    );
}

/// 验证：无 favicon 404；jpeg 能被发现但无 MIME 映射 → 415。
#[tokio::test]
async fn discover_icon_reports_missing_and_unsupported_favicons() {
    let data_dir = temp_dir_unique("icon-discover-404");
    let project_dir = data_dir.join("repo");
    let id = seed_project(&data_dir, &project_dir, Value::Null);

    let app = icon_app_router(&data_dir);
    let response = app
        .clone()
        .oneshot(request_json(
            "POST",
            &format!("/api/projects/{id}/icon/discover"),
            Some(("{}", "application/json")),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        body_json(response).await,
        json!({ "error": "No favicon found in project" })
    );

    // `favicon.jpeg` matches the discovery regex but has no MIME mapping
    // (the JS extension map covers png/jpg/svg/webp/ico only) → 415.
    std::fs::write(project_dir.join("favicon.jpeg"), b"jpeg").unwrap();
    let response = app
        .oneshot(request_json(
            "POST",
            &format!("/api/projects/{id}/icon/discover"),
            Some(("{}", "application/json")),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(
        body_json(response).await,
        json!({ "error": "Unsupported favicon format" })
    );
}

/// 验证：发现到空文件 favicon 时以 400 拒绝。
#[tokio::test]
async fn discover_icon_rejects_empty_favicons() {
    let data_dir = temp_dir_unique("icon-discover-empty");
    let project_dir = data_dir.join("repo");
    let id = seed_project(&data_dir, &project_dir, Value::Null);
    std::fs::write(project_dir.join("favicon.ico"), b"").unwrap();

    let app = icon_app_router(&data_dir);
    let response = app
        .oneshot(request_json(
            "POST",
            &format!("/api/projects/{id}/icon/discover"),
            Some(("{}", "application/json")),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_json(response).await,
        json!({ "error": "Discovered icon is empty" })
    );
}

// ---------------------------------------------------------------------------
// openchamber-routes.js — routes
// ---------------------------------------------------------------------------

/// 构造 GitHub release fake transport：releases/latest 返回给定 tag，
/// CHANGELOG.md 返回给定状态码。
fn release_transport(
    tag: &str,
    changelog_status: u16,
) -> Arc<crate::package_manager::http::testing::FakeTransport> {
    use crate::package_manager::http::testing::FakeTransport;
    let transport = FakeTransport::new();
    transport.when(
        "releases/latest",
        FakeTransport::ok_json(json!({ "tag_name": tag })),
    );
    transport.when("CHANGELOG.md", FakeTransport::status(changelog_status));
    Arc::clone(&transport)
}
/// 用共享 harness 组装完整的 meta 路由（openchamber 路由测试用）。
fn meta_app(
    data_dir: &Path,
    models: Arc<ModelsMetadataCache>,
    package_manager: Arc<PackageManagerRuntime>,
) -> axum::Router {
    super::router_with(test_context(data_dir), models, package_manager)
}

/// 验证：update-check 报告可用更新并附 releaseUrl/updateCommand，changelog 404 时无 body。
#[tokio::test]
async fn update_check_reports_an_available_release() {
    let data_dir = temp_dir_unique("uc-available");
    let transport = release_transport("v2.0.0", 404);
    let app = meta_app(
        &data_dir,
        fake_catalog(
            failing_fetch(HttpError::Other("unused".into())),
            data_dir.join("m.json"),
        ),
        fake_package_manager(Arc::clone(&transport)),
    );

    let response = app
        .oneshot(request_json(
            "GET",
            "/api/ompchamber/update-check?currentVersion=1.0.0&appType=web&reportUsage=false",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let payload = body_json(response).await;
    assert_eq!(payload.get("available"), Some(&json!(true)));
    assert_eq!(payload.get("version"), Some(&json!("2.0.0")));
    assert_eq!(payload.get("currentVersion"), Some(&json!("1.0.0")));
    assert_eq!(
        payload.get("releaseUrl"),
        Some(&json!(
            "https://github.com/Reamd7/ompchamber/releases/tag/v2.0.0"
        ))
    );
    assert_eq!(payload.get("packageManager"), Some(&json!("npm")));
    assert_eq!(
        payload.get("updateCommand"),
        Some(&json!("ompchamber update"))
    );
    assert!(payload.get("body").is_none(), "changelog 404 → no body");
    assert!(payload.get("downloadUrl").is_none());
}

/// 验证：changelog 可取时 body 只包含当前版本之后的段落。
#[tokio::test]
async fn update_check_includes_changelog_notes_when_fetchable() {
    let data_dir = temp_dir_unique("uc-changelog");
    use crate::package_manager::http::testing::FakeTransport;
    let transport = FakeTransport::new();
    transport.when(
        "releases/latest",
        FakeTransport::ok_json(json!({ "tag_name": "v1.1.0" })),
    );
    transport.when(
        "CHANGELOG.md",
        FakeTransport::ok_text("## [1.0.0]\nold\n\n## [1.1.0]\nnew features\n"),
    );
    let app = meta_app(
        &data_dir,
        fake_catalog(
            failing_fetch(HttpError::Other("unused".into())),
            data_dir.join("m.json"),
        ),
        fake_package_manager(Arc::clone(&transport)),
    );

    let response = app
        .oneshot(request_json(
            "GET",
            "/api/ompchamber/update-check?currentVersion=1.0.0",
            None,
        ))
        .await
        .unwrap();
    let payload = body_json(response).await;
    let body = payload.get("body").unwrap().as_str().unwrap();
    assert!(
        body.contains("[1.1.0]"),
        "only sections after the current version: {body}"
    );
    assert!(!body.contains("[1.0.0]"));
}

/// 验证：release 信息取不到时回答 "Unable to determine versions" 而非报错。
#[tokio::test]
async fn update_check_without_releases_answers_unable_to_determine() {
    let data_dir = temp_dir_unique("uc-none");
    use crate::package_manager::http::testing::FakeTransport;
    let transport = FakeTransport::new();
    transport.when("releases/latest", FakeTransport::reject("network down"));
    let app = meta_app(
        &data_dir,
        fake_catalog(
            failing_fetch(HttpError::Other("unused".into())),
            data_dir.join("m.json"),
        ),
        fake_package_manager(Arc::clone(&transport)),
    );

    let response = app
        .oneshot(request_json(
            "GET",
            "/api/ompchamber/update-check?currentVersion=1.0.0",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        body_json(response).await,
        json!({
            "available": false,
            "currentVersion": "1.0.0",
            "error": "Unable to determine versions",
        })
    );
}

/// 验证：无可用更新时 update-install 返回 400 "No update available"。
#[tokio::test]
async fn update_install_rejects_when_no_update_is_available() {
    let data_dir = temp_dir_unique("ui-400");
    let transport = release_transport("v0.0.1", 404); // older than package.json
    let app = meta_app(
        &data_dir,
        fake_catalog(
            failing_fetch(HttpError::Other("unused".into())),
            data_dir.join("m.json"),
        ),
        fake_package_manager(Arc::clone(&transport)),
    );

    let response = app
        .oneshot(request_json("POST", "/api/ompchamber/update-install", None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_json(response).await,
        json!({ "error": "No update available" })
    );
}

/// 验证：daemon 模式下 update-install 返回 409 与“本构建不可自更新”文案。
#[tokio::test]
async fn update_install_daemon_mode_answers_the_honest_not_available_shape() {
    let data_dir = temp_dir_unique("ui-daemon");
    let transport = release_transport("v9.9.9", 404);
    let app = meta_app(
        &data_dir,
        fake_catalog(
            failing_fetch(HttpError::Other("unused".into())),
            data_dir.join("m.json"),
        ),
        fake_package_manager(Arc::clone(&transport)),
    );

    // No `run/ompchamber-<port>.json` instance file → daemon launch mode.
    let response = app
        .oneshot(request_json("POST", "/api/ompchamber/update-install", None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let payload = body_json(response).await;
    assert!(
        payload["error"]
            .as_str()
            .unwrap()
            .contains("not available in this server build"),
        "honest not-available semantics: {payload}"
    );
}

/// 验证：前台且无 systemd unit 时返回与 JS 逐字一致的 409 拒绝文案。
#[tokio::test]
async fn update_install_foreground_without_systemd_keeps_the_exact_js_409() {
    let data_dir = temp_dir_unique("ui-fg");
    let transport = release_transport("v9.9.9", 404);
    let app = meta_app(
        &data_dir,
        fake_catalog(
            failing_fetch(HttpError::Other("unused".into())),
            data_dir.join("m.json"),
        ),
        fake_package_manager(Arc::clone(&transport)),
    );

    // Foreground launch mode without a systemd unit (INVOCATION_ID unset in
    // the test environment) → the JS's exact refusal.
    std::fs::create_dir_all(data_dir.join("run")).unwrap();
    std::fs::write(
        data_dir.join("run/ompchamber-7897.json"),
        r#"{ "launchMode": "foreground", "port": 7897 }"#,
    )
    .unwrap();

    let response = app
        .oneshot(request_json("POST", "/api/ompchamber/update-install", None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        body_json(response).await,
        json!({
            "error": "Foreground servers must be updated by their service manager. Set OMPCHAMBER_SYSTEMD_UNIT when running under systemd, or run ompchamber update and restart the service.",
        })
    );
}

// ---------------------------------------------------------------------------
// models-metadata + zen routes
// ---------------------------------------------------------------------------

/// 验证：models-metadata 路由新鲜响应 max-age=300、缓存命中 max-age=60，并落盘。
#[tokio::test]
async fn models_metadata_route_serves_fresh_then_cached_headers() {
    let data_dir = temp_dir_unique("mm-route");
    let cache_path = data_dir.join("models-dev.catalog.json");
    let (fetch, _requests, _calls) = recording_fetch(ok_response(
        200,
        vec![("etag", "\"e9\"")],
        r#"{"openai": {"models": {}}}"#,
    ));
    let app = meta_app(
        &data_dir,
        fake_catalog(fetch, cache_path.clone()),
        fake_package_manager(crate::package_manager::http::testing::FakeTransport::new()),
    );

    let response = app
        .clone()
        .oneshot(request_json("GET", "/api/ompchamber/models-metadata", None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CACHE_CONTROL).unwrap(),
        "public, max-age=300",
        "fresh network response"
    );
    assert_eq!(
        body_json(response).await,
        json!({ "openai": { "models": {} } })
    );

    let response = app
        .oneshot(request_json("GET", "/api/ompchamber/models-metadata", None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CACHE_CONTROL).unwrap(),
        "public, max-age=60",
        "in-memory cache hit"
    );
    assert!(cache_path.exists(), "disk cache populated");
}

/// 验证：直连超时且无 proxy 时最终错误是 "no proxy configured"，路由按 JS 映射为 502。
#[tokio::test]
async fn models_metadata_route_maps_a_direct_timeout_to_502_like_the_js() {
    // JS fidelity: the JS `fetchCatalog` runs the proxy attempt after a
    // direct timeout and, with no proxies configured, propagates
    // `'no proxy configured'` as the final error — a plain Error whose name
    // is neither TimeoutError nor AbortError, so the route answers 502. The
    // 504 mapping only fires when a timeout error is the final survivor.
    let data_dir = temp_dir_unique("mm-timeout");
    let app = meta_app(
        &data_dir,
        fake_catalog(
            failing_fetch(HttpError::Timeout),
            data_dir.join("models-dev.catalog.json"),
        ),
        fake_package_manager(crate::package_manager::http::testing::FakeTransport::new()),
    );

    let response = app
        .oneshot(request_json("GET", "/api/ompchamber/models-metadata", None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        body_json(response).await,
        json!({ "error": "Failed to retrieve model metadata" })
    );
}

/// 验证：非超时类失败统一映射为 502。
#[tokio::test]
async fn models_metadata_route_maps_other_failures_to_502() {
    let data_dir = temp_dir_unique("mm-502");
    let app = meta_app(
        &data_dir,
        fake_catalog(
            failing_fetch(HttpError::Other("socket hang up".into())),
            data_dir.join("models-dev.catalog.json"),
        ),
        fake_package_manager(crate::package_manager::http::testing::FakeTransport::new()),
    );

    let response = app
        .oneshot(request_json("GET", "/api/ompchamber/models-metadata", None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
}

/// 验证：zen models 恒返回空列表与 300 秒 Cache-Control。
#[tokio::test]
async fn zen_models_serves_the_stubbed_empty_list() {
    let data_dir = temp_dir_unique("zen");
    let app = meta_app(
        &data_dir,
        fake_catalog(
            failing_fetch(HttpError::Other("unused".into())),
            data_dir.join("m.json"),
        ),
        fake_package_manager(crate::package_manager::http::testing::FakeTransport::new()),
    );

    let response = app
        .oneshot(request_json("GET", "/api/zen/models", None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CACHE_CONTROL).unwrap(),
        "public, max-age=300"
    );
    assert_eq!(body_json(response).await, json!({ "models": [] }));
}
