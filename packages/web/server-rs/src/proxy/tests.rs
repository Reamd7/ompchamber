//! Route-level tests for the proxy port. Upstreams are raw `tokio` TCP
//! listeners serving canned HTTP/SSE bytes, mirroring the JS
//! `opencode-proxy.test.js` setups.
//!
//! 中文说明：proxy 端口的路由级测试。上游是裸 tokio TCP listener，按
//! 脚本回放预置的 HTTP/SSE 字节，对应 JS 版 opencode-proxy.test.js 的
//! 搭建方式。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request as HttpRequest, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::ServiceExt;

use crate::config::{EngineConfig, ServerConfig, TunnelOptions};
use crate::context::RouterContext;
use crate::engine::EngineState;
use crate::hub::EventHub;
use crate::proxy::{ProxySettings, build_router};

// ---------------------------------------------------------------------------
// Test scaffolding
// ---------------------------------------------------------------------------

/// 测试用 proxy 参数：零就绪宽限、4 秒预算；SSE 心跳/看门狗由各用例
/// 自行覆盖。
fn test_settings() -> ProxySettings {
    ProxySettings {
        ready_grace_ms: 0,
        request_timeout_ms: 4_000,
        oauth_timeout_ms: 4_000,
        turn_bound_timeout_ms: 4_000,
        sse_heartbeat_ms: 20_000,
        sse_stall_ms: 20_000,
        fallback_target: "http://127.0.0.1:3902".to_string(),
    }
}

/// 构造测试 RouterContext：临时目录、外部引擎指向 127.0.0.1:1，
/// 各用例再用真实上游端口覆盖。
fn test_ctx(engine: Arc<EngineState>, hub: Arc<EventHub>) -> RouterContext {
    RouterContext {
        config: Arc::new(ServerConfig {
            port: 0,
            host: None,
            lan: false,
            ui_password: None,
            api_only: false,
            data_dir: std::env::temp_dir(),
            dist_dir: std::env::temp_dir(),
            tunnel: TunnelOptions::default(),
            engine: EngineConfig::External {
                base_url: "http://127.0.0.1:1".to_string(),
            },
        }),
        engine,
        hub,
    }
}

/// 构造已就绪的外部引擎状态（可带引擎密码）。
fn ready_engine(port: u16, password: Option<&str>) -> Arc<EngineState> {
    EngineState::external(
        format!("http://127.0.0.1:{port}"),
        password.map(str::to_string),
    )
}

/// One canned HTTP response with a body (connection closes after write).
/// 中文：拼一条带 content-length 的完整 HTTP 响应字节（写完即关连接）。
fn http_response(status: u16, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        _ => "Whatever",
    };
    let mut head = format!("HTTP/1.1 {status} {reason}\r\n");
    for (key, value) in headers {
        head.push_str(&format!("{key}: {value}\r\n"));
    }
    head.push_str(&format!(
        "content-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    ));
    let mut out = head.into_bytes();
    out.extend_from_slice(body);
    out
}

/// An SSE response head (no content-length, stream stays open).
/// 中文：SSE 响应头（无 content-length，连接保持打开）。
fn sse_response_head() -> Vec<u8> {
    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: keep-alive\r\n\r\n".to_vec()
}

/// 在字节串中查找子串首次出现的位置，供脚本上游定位请求头结束处。
fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// A scripted upstream: accepts connections forever, captures each raw
/// request (head+body), replies with `initial`, then plays `delayed` frames
/// (delay in ms before each write), then stays open for `hold_open_ms` before
/// closing — enough to emulate streaming SSE or silent upstreams.
/// 中文：脚本化上游——循环接受连接，捕获每个原始请求，先回 initial，
/// 再按各自延迟逐帧写 delayed，最后保持连接 hold_open_ms 再关闭；足以
/// 模拟流式 SSE 或静默上游。返回端口与捕获的请求列表。
async fn spawn_scripted_upstream(
    initial: Vec<u8>,
    delayed: Vec<(u64, Vec<u8>)>,
    hold_open_ms: u64,
) -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let port = listener.local_addr().unwrap().port();
    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = captured.clone();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let sink = sink.clone();
            let initial = initial.clone();
            let delayed = delayed.clone();
            tokio::spawn(async move {
                let mut buf: Vec<u8> = Vec::new();
                let mut chunk = [0u8; 8192];
                loop {
                    if let Some(header_end) = find_subsequence(&buf, b"\r\n\r\n").map(|pos| pos + 4)
                    {
                        let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
                        let content_length = head
                            .to_ascii_lowercase()
                            .lines()
                            .find_map(|line| line.trim().strip_prefix("content-length:"))
                            .and_then(|value| value.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if buf.len() >= header_end + content_length {
                            break;
                        }
                    }
                    match tokio::time::timeout(Duration::from_secs(2), socket.read(&mut chunk))
                        .await
                    {
                        Ok(Ok(0)) | Err(_) | Ok(Err(_)) => break,
                        Ok(Ok(n)) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                sink.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(String::from_utf8_lossy(&buf).into_owned());
                let _ = socket.write_all(&initial).await;
                for (delay, frame) in delayed {
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    if socket.write_all(&frame).await.is_err() {
                        break;
                    }
                }
                if hold_open_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(hold_open_ms)).await;
                }
                let _ = socket.shutdown().await;
            });
        }
    });
    (port, captured)
}

/// 读全响应体为字节。
async fn read_body(response: axum::response::Response) -> Vec<u8> {
    to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read response body")
        .to_vec()
}

/// 读全响应体并按 JSON 解析。
async fn json_body(response: axum::response::Response) -> serde_json::Value {
    let bytes = read_body(response).await;
    serde_json::from_slice(&bytes).expect("json body")
}

/// 取最近一个被捕获请求的 (head, body) 二元组。
fn captured_request(captured: &Arc<Mutex<Vec<String>>>) -> (String, String) {
    let requests = captured.lock().unwrap_or_else(|e| e.into_inner());
    let raw = requests.last().expect("upstream saw a request").clone();
    let (head, body) = raw.split_once("\r\n\r\n").expect("request head/body split");
    (head.to_string(), body.to_string())
}

// ---------------------------------------------------------------------------
// Generic proxy
// ---------------------------------------------------------------------------

/// 验证：通用转发保留方法/路径/头，客户端凭据被引擎鉴权替换，请求体
/// 与查询串原样到达上游。
#[tokio::test]
async fn generic_proxy_forwards_method_path_headers_and_auth() {
    let response = http_response(
        200,
        &[
            ("content-type", "application/json"),
            ("x-upstream-test", "ok"),
        ],
        br#"{"ok":true}"#,
    );
    let (port, captured) = spawn_scripted_upstream(response, vec![], 0).await;
    let router: Router = build_router(
        test_ctx(ready_engine(port, Some("test-password")), EventHub::new()),
        test_settings(),
    );

    let request = HttpRequest::builder()
        .method("POST")
        .uri("/api/config/providers?scope=user")
        .header("content-type", "application/json")
        .header("authorization", "Bearer stale-client-token")
        .header("accept-encoding", "gzip, deflate, br")
        .header("x-custom", "kept")
        .body(Body::from(br#"{"a":1}"#.to_vec()))
        .unwrap();
    let response = router.oneshot(request).await.expect("oneshot");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers().get("x-upstream-test").unwrap(), "ok");
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/json"
    );
    assert_eq!(read_body(response).await, br#"{"ok":true}"#.to_vec());

    let (head, body) = captured_request(&captured);
    let lowered = head.to_ascii_lowercase();
    assert!(
        head.starts_with("POST /config/providers?scope=user HTTP/1.1"),
        "{head}"
    );
    let expected_auth = format!("Basic {}", BASE64.encode("opencode:test-password"));
    assert!(lowered.contains(&format!("authorization: {expected_auth}").to_ascii_lowercase()));
    assert!(!lowered.contains("authorization: bearer"));
    assert!(lowered.contains("accept-encoding: identity"));
    assert!(lowered.contains("x-custom: kept"));
    assert!(lowered.contains(&format!("content-length: {}", body.len())));
    assert_eq!(body, r#"{"a":1}"#);
}

/// 验证：响应中的 hop-by-hop 与 content-encoding 等头被剥离，普通头保留。
#[tokio::test]
async fn generic_proxy_strips_hop_by_hop_response_headers() {
    let response = http_response(
        200,
        &[
            ("content-type", "application/json"),
            ("etag", "W/\"abc\""),
            ("content-encoding", "gzip"),
            ("www-authenticate", "Basic realm=\"x\""),
        ],
        br#"{}"#,
    );
    let (port, _captured) = spawn_scripted_upstream(response, vec![], 0).await;
    let router = build_router(
        test_ctx(ready_engine(port, None), EventHub::new()),
        test_settings(),
    );

    let response = router
        .oneshot(
            HttpRequest::get("/api/config/providers")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers().get("etag").unwrap(), "W/\"abc\"");
    assert!(response.headers().get("content-encoding").is_none());
    assert!(response.headers().get("www-authenticate").is_none());
    assert!(response.headers().get("content-length").is_none());
}

/// 验证：引擎始终未就绪时，就绪门在宽限耗尽后返回 503 与 restarting
/// 错误体。
#[tokio::test]
async fn readiness_gate_returns_503_restarting_when_engine_never_becomes_ready() {
    let engine = EngineState::external("http://127.0.0.1:1".to_string(), None);
    engine.shutdown().await; // marks the engine not-ready with no base URL
    // Zero grace (JS test 'returns 503 fast when OpenCode never becomes ready').
    let router = build_router(test_ctx(engine, EventHub::new()), test_settings());

    let response = router
        .oneshot(
            HttpRequest::get("/api/config/providers")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let value = json_body(response).await;
    assert_eq!(value["error"], "OpenCode is restarting");
    assert_eq!(value["restarting"], true);
}

/// 验证：豁免路径跳过就绪扣留并转发到兜底目标。
#[tokio::test]
async fn gate_exempt_paths_skip_the_readiness_hold_and_use_fallback_target() {
    let response = http_response(
        200,
        &[("content-type", "application/json")],
        br#"{"healthy":true}"#,
    );
    let (fallback_port, captured) = spawn_scripted_upstream(response, vec![], 0).await;
    let engine = EngineState::external("http://127.0.0.1:1".to_string(), None);
    engine.shutdown().await;

    let mut settings = test_settings();
    settings.fallback_target = format!("http://127.0.0.1:{fallback_port}");
    let router = build_router(test_ctx(engine, EventHub::new()), settings);

    let response = router
        .oneshot(HttpRequest::get("/api/health").body(Body::empty()).unwrap())
        .await
        .unwrap();

    // Exempt path skipped the hold and proxied to the fallback target.
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await["healthy"], true);
    let (head, _) = captured_request(&captured);
    assert!(head.starts_with("GET /health HTTP/1.1"), "{head}");
}

/// 验证：上游超时返回 504 与 JS 同款错误体。
#[tokio::test]
async fn upstream_timeout_answers_504_with_js_error_shape() {
    // Silent upstream: accepts, reads the request, never writes.
    let (port, _captured) = spawn_scripted_upstream(Vec::new(), vec![], 5_000).await;
    let mut settings = test_settings();
    settings.request_timeout_ms = 150;
    let router = build_router(
        test_ctx(ready_engine(port, None), EventHub::new()),
        settings,
    );

    let response = router
        .oneshot(HttpRequest::get("/api/slow").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    let value = json_body(response).await;
    assert_eq!(value["error"], "OpenCode upstream timed out");
}

/// 验证：OAuth 回调、会话动作与深层会话路径都按去 /api 前缀后的路径
/// 转发到上游。
#[tokio::test]
async fn oauth_and_session_action_routes_forward_with_matching_paths() {
    let response = http_response(200, &[("content-type", "application/json")], b"true");
    let (port, captured) = spawn_scripted_upstream(response, vec![], 0).await;
    let router = build_router(
        test_ctx(ready_engine(port, None), EventHub::new()),
        test_settings(),
    );

    let oauth = router
        .clone()
        .oneshot(
            HttpRequest::builder()
                .method("POST")
                .uri("/api/provider/github-copilot/oauth/callback")
                .header("content-type", "application/json")
                .body(Body::from(b"{\"method\":0}".to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(oauth.status(), StatusCode::OK);
    let (head, _) = captured_request(&captured);
    assert!(
        head.starts_with("POST /provider/github-copilot/oauth/callback HTTP/1.1"),
        "{head}"
    );

    // Non-turn-bound session actions fall through to the generic proxy.
    let revert = router
        .clone()
        .oneshot(
            HttpRequest::builder()
                .method("POST")
                .uri("/api/session/ses_1/revert")
                .header("content-type", "application/json")
                .body(Body::from(b"{\"messageID\":\"msg_1\"}".to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(revert.status(), StatusCode::OK);
    let (head, _) = captured_request(&captured);
    assert!(
        head.starts_with("POST /session/ses_1/revert HTTP/1.1"),
        "{head}"
    );

    // Deep session paths (beyond the two-segment action route) go generic.
    let messages = router
        .clone()
        .oneshot(
            HttpRequest::get("/api/session/ses_1/message?limit=5")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(messages.status(), StatusCode::OK);
    let (head, _) = captured_request(&captured);
    assert!(
        head.starts_with("GET /session/ses_1/message?limit=5 HTTP/1.1"),
        "{head}"
    );

    // Turn-bound actions are forwarded on the same wire path.
    let prompt = router
        .oneshot(
            HttpRequest::builder()
                .method("POST")
                .uri("/api/session/ses_1/prompt_async")
                .header("content-type", "application/json")
                .body(Body::from(b"{\"parts\":[]}".to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(prompt.status(), StatusCode::OK);
    let (head, _) = captured_request(&captured);
    assert!(
        head.starts_with("POST /session/ses_1/prompt_async HTTP/1.1"),
        "{head}"
    );
}

// ---------------------------------------------------------------------------
// Sanitized session list
// ---------------------------------------------------------------------------

/// 验证：会话列表脱敏字段，并把 directory 查询参数规范化后发给上游。
#[tokio::test]
async fn session_list_sanitizes_payloads_and_canonicalizes_directory_query() {
    let payload = br#" [{
        "id": "ses_1",
        "directory": "/repo/app",
        "title": "Alpha",
        "time": {"created": 1, "updated": 2},
        "summary": {"additions": 5, "deletions": 3, "files": 2, "diffs": [{"patch": "@@ -1 +1 @@"}]},
        "metadata": {"custom": {"value": "kept"}},
        "revert": {"messageID": "msg_1", "partID": "part_1", "snapshot": "abc", "diff": "..."},
        "permission": [{"permission": "todowrite", "action": "deny", "pattern": "*"}]
    } ] "#
    .to_vec();
    let response = http_response(
        200,
        &[
            ("content-type", "application/json"),
            ("x-next-cursor", "123"),
        ],
        &payload,
    );
    let (port, captured) = spawn_scripted_upstream(response, vec![], 0).await;

    // A real temp directory: canonicalize() resolves it (on macOS TMPDIR is
    // a symlink into /private/var, so the rewrite is observable).
    let directory =
        std::env::temp_dir().join(format!("ompchamber-proxy-list-{}", std::process::id()));
    std::fs::create_dir_all(&directory).expect("create temp dir");
    let canonical = std::fs::canonicalize(&directory).expect("canonicalize");
    let encoded = crate::proxy::headers::percent_encode_form(&directory.to_string_lossy());

    let router = build_router(
        test_ctx(ready_engine(port, Some("session-token")), EventHub::new()),
        test_settings(),
    );
    let response = router
        .oneshot(
            HttpRequest::get(format!("/api/session?directory={encoded}&limit=500"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers().get("x-next-cursor").unwrap(), "123");
    assert!(
        response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("application/json"),
        "content-type preserved"
    );
    let value = json_body(response).await;
    let expected = serde_json::json!([{
        "id": "ses_1",
        "directory": "/repo/app",
        "title": "Alpha",
        "time": {"created": 1, "updated": 2},
        "summary": {"additions": 5, "deletions": 3, "files": 2},
        "metadata": {"custom": {"value": "kept"}},
        "revert": {"messageID": "msg_1", "partID": "part_1"},
    }]);
    assert_eq!(value, expected);

    // The upstream saw the canonicalized directory + engine auth.
    let (head, _) = captured_request(&captured);
    let expected_directory =
        crate::proxy::headers::percent_encode_form(&canonical.to_string_lossy());
    assert!(
        head.starts_with(&format!(
            "GET /session?directory={expected_directory}&limit=500 HTTP/1.1"
        )),
        "{head}"
    );
    let expected_auth = format!("Basic {}", BASE64.encode("opencode:session-token"));
    assert!(
        head.to_ascii_lowercase()
            .contains(&format!("authorization: {expected_auth}").to_ascii_lowercase())
    );
    let _ = std::fs::remove_dir_all(&directory);
}

/// 验证：非数组 JSON 与错误响应按原文透传，不做脱敏。
#[tokio::test]
async fn session_list_passes_non_array_json_and_errors_through_verbatim() {
    let body = br#"{"error":"not a list"}"#.to_vec();
    let response = http_response(400, &[("content-type", "application/json")], &body);
    let (port, _captured) = spawn_scripted_upstream(response.clone(), vec![], 0).await;
    let router = build_router(
        test_ctx(ready_engine(port, None), EventHub::new()),
        test_settings(),
    );

    let response = router
        .oneshot(
            HttpRequest::get("/api/session")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(read_body(response).await, body);

    // Broken JSON on the list route passes through as text, too.
    let broken = http_response(
        200,
        &[("content-type", "application/json")],
        b"not json at all",
    );
    let (port, _captured) = spawn_scripted_upstream(broken.clone(), vec![], 0).await;
    let router = build_router(
        test_ctx(ready_engine(port, None), EventHub::new()),
        test_settings(),
    );
    let response = router
        .oneshot(
            HttpRequest::get("/api/experimental/session")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(read_body(response).await, b"not json at all".to_vec());
}

/// 验证：会话详情路径走通用转发，不脱敏。
#[tokio::test]
async fn session_detail_responses_are_not_sanitized() {
    let payload = br#" {"id":"abc","summary":{"diffs":[{"patch":"@@ -1 +1 @@"}]},"revert":{"messageID":"msg_1","snapshot":"abc","diff":"..."}} "#.to_vec();
    let response = http_response(200, &[("content-type", "application/json")], &payload);
    let (port, _captured) = spawn_scripted_upstream(response, vec![], 0).await;
    let router = build_router(
        test_ctx(ready_engine(port, None), EventHub::new()),
        test_settings(),
    );

    // /api/session/abc is NOT the list route — generic proxy, no sanitization.
    let response = router
        .oneshot(
            HttpRequest::get("/api/session/abc")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let value = json_body(response).await;
    assert_eq!(value["summary"]["diffs"][0]["patch"], "@@ -1 +1 @@");
    assert_eq!(value["revert"]["snapshot"], "abc");
}

// ---------------------------------------------------------------------------
// SSE forwarding
// ---------------------------------------------------------------------------

/// 验证：SSE 透传上游帧与响应头，请求侧携带引擎鉴权。
#[tokio::test]
async fn sse_passes_upstream_frames_and_headers_through() {
    let delayed = vec![(20, b"data: {\"ok\":true}\n\n".to_vec())];
    let (port, captured) = spawn_scripted_upstream(sse_response_head(), delayed, 2_000).await;

    let mut settings = test_settings();
    settings.sse_heartbeat_ms = 20_000;
    settings.sse_stall_ms = 400;
    let router = build_router(
        test_ctx(ready_engine(port, Some("test-token")), EventHub::new()),
        settings,
    );

    let response = router
        .oneshot(
            HttpRequest::get("/api/global/event")
                .header("accept", "text/event-stream")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("text/event-stream"),
        "upstream content-type preserved"
    );
    assert_eq!(response.headers().get("cache-control").unwrap(), "no-cache");
    assert_eq!(response.headers().get("x-accel-buffering").unwrap(), "no");

    let body = String::from_utf8(read_body(response).await).expect("sse body");
    assert!(body.contains("data: {\"ok\":true}\n\n"), "{body}");

    // The upstream request carried the engine auth and SSE defaults.
    let (head, _) = captured_request(&captured);
    assert!(head.starts_with("GET /global/event HTTP/1.1"), "{head}");
    let expected_auth = format!("Basic {}", BASE64.encode("opencode:test-token"));
    assert!(
        head.to_ascii_lowercase()
            .contains(&format!("authorization: {expected_auth}").to_ascii_lowercase())
    );
}

/// 验证：hub 帧按顺序合入上游帧之间，心跳保持流活跃。
#[tokio::test]
async fn sse_merges_hub_frames_between_upstream_frames_in_order() {
    let delayed = vec![
        (30, b"data: {\"first\":true}\n\n".to_vec()),
        (260, b"data: {\"second\":true}\n\n".to_vec()),
    ];
    let (port, _captured) = spawn_scripted_upstream(sse_response_head(), delayed, 1_500).await;

    let hub = EventHub::new();
    let mut settings = test_settings();
    settings.sse_heartbeat_ms = 60;
    settings.sse_stall_ms = 1_200;
    let router = build_router(test_ctx(ready_engine(port, None), hub.clone()), settings);

    // Publish a server-emitted frame between the two upstream frames.
    let publisher = hub.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(120)).await;
        publisher.publish_json(
            "ompchamber:session-status",
            &serde_json::json!({"sessionID": "ses_1", "status": "idle"}),
        );
    });

    let response = router
        .oneshot(HttpRequest::get("/api/event").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = String::from_utf8(read_body(response).await).expect("sse body");
    let first = body.find("\"first\"").expect("first frame");
    let merged = body.find("ompchamber:session-status").expect("hub frame");
    let second = body.find("\"second\"").expect("second frame");
    assert!(first < merged && merged < second, "merged in order: {body}");
    assert!(
        body.contains("event: ompchamber:session-status\ndata: {\"sessionID\":\"ses_1\",\"status\":\"idle\"}\n\n"),
        "{body}"
    );
    assert!(
        body.contains(":heartbeat\n\n"),
        "heartbeats keep the stream alive: {body}"
    );
}

/// 验证：上游静默时看门狗结束响应，期间心跳照发。
#[tokio::test]
async fn sse_stall_watchdog_closes_the_response_when_upstream_goes_silent() {
    let delayed = vec![(20, b"data: {\"only\":true}\n\n".to_vec())];
    let (port, _captured) = spawn_scripted_upstream(sse_response_head(), delayed, 3_000).await;

    let mut settings = test_settings();
    settings.sse_heartbeat_ms = 25;
    settings.sse_stall_ms = 150;
    let router = build_router(
        test_ctx(ready_engine(port, None), EventHub::new()),
        settings,
    );

    let response = router
        .oneshot(HttpRequest::get("/api/event").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Completes because the stall watchdog ends the body despite the
    // upstream holding the socket open, and our heartbeats kept flowing.
    let body = String::from_utf8(read_body(response).await).expect("sse body");
    assert!(body.contains("data: {\"only\":true}\n\n"), "{body}");
    assert!(body.contains(":heartbeat\n\n"), "{body}");
}

/// 验证：事件端点对非事件流响应按普通字节流回传。
#[tokio::test]
async fn sse_endpoint_passes_non_event_stream_responses_through() {
    let response = http_response(
        400,
        &[("content-type", "application/json")],
        br#"{"error":"nope"}"#,
    );
    let (port, _captured) = spawn_scripted_upstream(response, vec![], 0).await;
    let router = build_router(
        test_ctx(ready_engine(port, None), EventHub::new()),
        test_settings(),
    );

    let response = router
        .oneshot(HttpRequest::get("/api/event").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        String::from_utf8(read_body(response).await).unwrap(),
        r#"{"error":"nope"}"#
    );
}

/// 验证：引擎未就绪时事件端点被就绪门拦下。
#[tokio::test]
async fn sse_gate_blocks_requests_while_engine_is_not_ready() {
    let engine = EngineState::external("http://127.0.0.1:1".to_string(), None);
    engine.shutdown().await;
    let router = build_router(test_ctx(engine, EventHub::new()), test_settings());

    let response = router
        .oneshot(HttpRequest::get("/api/event").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let value = json_body(response).await;
    assert_eq!(value["error"], "OpenCode is restarting");
}
