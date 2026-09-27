//! Port of `server/lib/browser-control/routes.js` — the result callback for
//! in-app browser actions.
//!
//! The client that owns the browser view posts here with the outcome of a
//! request it received over the event stream. Only the request id is trusted
//! to correlate; an unknown id is accepted with `matched: false` rather than
//! an error, because a client answering after a timeout has done nothing
//! wrong.
//!
//! Route map (paths, verbs, JSON shapes mirror routes.js exactly):
//! - `POST /api/browser-control/claim`  (4 kb body) → `{ granted }` / 400
//! - `POST /api/browser-control/result` (2 MB body) → `{ matched }` / 400
//!
//! 中文说明：本文件是 `server/lib/browser-control/routes.js` 的移植。持有浏览器视图的
//! 客户端在此回传动作结果；仅凭 request id 关联，未知 id 返回 `matched: false` 而非
//! 报错——超时后才应答的客户端并没有做错什么。

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::browser_control::broker::{BrowserControlBroker, ClientResult};
use crate::context::RouterContext;

/// `express.json({ limit: '4kb' })` on the claim route.
/// 中文：claim 只携带 request id，4 KB 足够。
const CLAIM_BODY_LIMIT_BYTES: usize = 4 * 1024;
/// `express.json({ limit: '2mb' })` on the result route. This server attaches
/// body parsing per route rather than globally, and the limit is sized for a
/// page snapshot (visible text plus every interactive element), not for a
/// small control message.
/// 中文：按页面快照的体量设计（可见文本加全部可交互元素）。
const RESULT_BODY_LIMIT_BYTES: usize = 2 * 1024 * 1024;

/// 路由级状态：持有与工具服务共享的 broker。
#[derive(Clone)]
struct ModuleState {
    /// 共享的浏览器控制 broker 实例。
    broker: Arc<BrowserControlBroker>,
}

/// Express's default unmatched-method page (verbatim) for GET on POST-only
/// paths — express answers 404, never 405.
/// 中文：返回 express 默认的 JSON 404 页面，路径去掉 `/api` 前缀。
async fn express_404_get(
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
) -> axum::response::Response {
    use axum::http::{StatusCode, header};
    use axum::response::IntoResponse;
    // The JS /api mount answers unmatched methods with its JSON 404, path
    // stripped of the /api prefix.
    let stripped = uri.path().strip_prefix("/api").unwrap_or(uri.path());
    (
        StatusCode::NOT_FOUND,
        [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
        axum::Json(serde_json::json!({
            "name": "UnknownError",
            "data": { "message": format!("Not found: GET {stripped}") }
        })),
    )
        .into_response()
}

/// `registerBrowserControlRoutes(app, { broker })` — composition entry for
/// main.rs: the caller owns the broker (shared with the `openchamber_web`
/// tool service, exactly as index.js threads one instance into both).
/// 中文：两个路由分别挂 4 KB / 2 MB 请求体上限，并套用 `/api` 鉴权中间件。
pub fn router_shared(ctx: RouterContext, broker: Arc<BrowserControlBroker>) -> Router {
    Router::new()
        .route(
            "/api/browser-control/claim",
            // Express has no GET route: unmatched methods fall through to the
            // express 404, not axum's 405. Mirror the express default page.
            get(express_404_get)
                .post(claim)
                .layer(DefaultBodyLimit::max(CLAIM_BODY_LIMIT_BYTES)),
        )
        .route(
            "/api/browser-control/result",
            get(express_404_get)
                .post(result)
                .layer(DefaultBodyLimit::max(RESULT_BODY_LIMIT_BYTES)),
        )
        // `/api` requests pass through `requireApiAuth` before any route.
        .route_layer(crate::ui_auth::middleware(ctx))
        .with_state(ModuleState { broker })
}

/// `express.json({ limit })` semantics for one route: parse only JSON-typed
/// bodies (`application/json` / `*+json`), leave everything else "unparsed"
/// (Express leaves `req.body` undefined, which the handlers treat as missing);
/// malformed JSON is a 400 before the handler runs, like body-parser.
/// 中文：非 JSON 类型返回 `Value::Null` 表示未解析；格式错误在进入处理器前转 400。
fn parse_json_body(headers: &HeaderMap, body: &Bytes) -> Result<Value, Response> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_lowercase();
    if content_type.starts_with("application/json") || content_type.ends_with("+json") {
        return serde_json::from_slice(body).map_err(|error| bad_request(&error.to_string()));
    }
    Ok(Value::Null)
}

/// 统一的 400 响应：`{"error": <message>}`。
fn bad_request(error: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))).into_response()
}

/// 取对象里的字符串字段；trim 后非空才返回，否则视为缺失。
fn string_field<'a>(body: &'a Value, key: &str) -> Option<&'a str> {
    body.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Claiming is separate from answering so that a client learns whether it
/// may act *before* it acts. Deciding by whose result arrives first would be
/// too late: by then every client has already clicked.
/// 中文：claim 与 result 分离，让客户端在动手之前就知道自己是否有权执行。
async fn claim(State(state): State<ModuleState>, headers: HeaderMap, body: Bytes) -> Response {
    let parsed = match parse_json_body(&headers, &body) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let Some(request_id) = string_field(&parsed, "requestId") else {
        return bad_request("requestId is required");
    };
    (
        StatusCode::OK,
        Json(json!({ "granted": state.broker.claim(request_id) })),
    )
        .into_response()
}

/// Accepts the outcome the owning client posts back. A snapshot carries the
/// visible text plus every interactive element, hence the 2 MB limit above.
/// 中文：信封校验（必须是对象/数组、必含 requestId）通过后交由 broker 收结。
async fn result(State(state): State<ModuleState>, headers: HeaderMap, body: Bytes) -> Response {
    let parsed = match parse_json_body(&headers, &body) {
        Ok(value) => value,
        Err(response) => return response,
    };
    // `typeof body === 'object'` — true for objects and arrays, false for
    // `null`/strings/numbers/booleans (`!body` catches null first in JS).
    if !matches!(parsed, Value::Object(_) | Value::Array(_)) {
        return bad_request("A JSON body is required");
    }
    let Some(request_id) = string_field(&parsed, "requestId") else {
        return bad_request("requestId is required");
    };

    let matched = state.broker.resolve(
        request_id,
        ClientResult {
            ok: parsed.get("ok").and_then(Value::as_bool) == Some(true),
            data: parsed.get("data").cloned().unwrap_or(Value::Null),
            error: parsed
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        },
    );
    (StatusCode::OK, Json(json!({ "matched": matched }))).into_response()
}

/// 路由层测试：覆盖 body 解析语义、claim/result 行为与请求体上限。
#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use tower::ServiceExt;

    use std::sync::Mutex;

    use crate::browser_control::broker::{OutgoingRequest, RequestOptions};
    use crate::config::{EngineConfig, ServerConfig};
    use crate::engine::EngineState;
    use crate::hub::EventHub;

    // These run against the full module router (gate included) on purpose:
    // a route that forgets its body parsing still registers fine and only
    // fails when a client posts to it — which surfaces to the agent as an
    // unexplained timeout, nowhere near the cause.
    /// 构造带临时数据目录的 RouterContext；故意走完整路由（含鉴权中间件）。
    fn test_context() -> RouterContext {
        let dir = std::env::temp_dir().join(format!(
            "ompchamber-browser-control-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        RouterContext {
            config: Arc::new(ServerConfig {
                port: 0,
                host: None,
                lan: false,
                ui_password: None,
                api_only: false,
                data_dir: dir.clone(),
                dist_dir: dir.join("dist"),
                tunnel: Default::default(),
                engine: EngineConfig::Managed {
                    hostname: "127.0.0.1".to_string(),
                },
            }),
            engine: EngineState::external("http://127.0.0.1:1".to_string(), None),
            hub: EventHub::new(),
        }
    }

    /// 构造会记录广播日志、返回固定监听数的 broker。
    fn broker_with_log(
        listeners: usize,
    ) -> (Arc<BrowserControlBroker>, Arc<Mutex<Vec<OutgoingRequest>>>) {
        let emitted = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&emitted);
        let broker = BrowserControlBroker::new(move |payload| {
            log.lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(payload.clone());
            listeners
        });
        (broker, emitted)
    }

    /// 取广播日志首条请求的 id。
    fn first_request_id(emitted: &Mutex<Vec<OutgoingRequest>>) -> String {
        emitted.lock().unwrap_or_else(|e| e.into_inner())[0]
            .request_id
            .clone()
    }

    /// Starts the request the route test will answer. `broker.request(...)`
    /// in JS emits synchronously before returning; the spawned task reaches
    /// that same point after one yield, so the request id is available.
    /// 中文：spawn 后让出一次，使请求已广播、id 可取。
    async fn start_inflight(
        broker: &Arc<BrowserControlBroker>,
        action: &str,
    ) -> tokio::task::JoinHandle<Result<Value, crate::browser_control::BrowserControlError>> {
        let handle = {
            let broker = Arc::clone(broker);
            let action = action.to_string();
            tokio::spawn(async move {
                broker
                    .request(action, json!({}), RequestOptions::default())
                    .await
            })
        };
        tokio::task::yield_now().await;
        handle
    }

    /// 以测试上下文与指定 broker 组装完整路由。
    fn app(broker: Arc<BrowserControlBroker>) -> Router {
        router_shared(test_context(), broker)
    }

    /// 以 JSON 请求体 POST 到指定路径，返回状态码与解析后的响应体。
    async fn post_json(app: Router, path: &str, raw_body: String) -> (StatusCode, Value) {
        let response = app
            .oneshot(
                axum::http::Request::post(path)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(raw_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    /// 客户端回传 JSON 后，等待中的请求以该数据收结。
    #[tokio::test]
    async fn parses_a_posted_json_body_and_resolves_the_waiting_request() {
        let (broker, emitted) = broker_with_log(1);
        let inflight = start_inflight(&broker, "browser.snapshot").await;
        let request_id = first_request_id(&emitted);

        let (status, body) = post_json(
            app(Arc::clone(&broker)),
            "/api/browser-control/result",
            json!({ "requestId": request_id, "ok": true, "data": { "url": "http://localhost:3000/" } })
                .to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({ "matched": true }));
        assert_eq!(
            inflight.await.unwrap().unwrap(),
            json!({ "url": "http://localhost:3000/" })
        );
    }

    /// 2 MB 上限足以承载真实页面的快照数据。
    #[tokio::test]
    async fn accepts_a_snapshot_large_enough_to_carry_a_real_page() {
        let (broker, emitted) = broker_with_log(1);
        let inflight = start_inflight(&broker, "browser.snapshot").await;
        let request_id = first_request_id(&emitted);

        let data = json!({
            "url": "http://localhost:3000/",
            "text": "x".repeat(200_000),
            "elements": (0..120).map(|index| json!({
                "selector": format!("div:nth-of-type({index})"),
                "label": "y".repeat(100),
            })).collect::<Vec<_>>(),
        });

        let (status, body) = post_json(
            app(Arc::clone(&broker)),
            "/api/browser-control/result",
            json!({ "requestId": request_id, "ok": true, "data": data }).to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({ "matched": true }));

        let result = inflight.await.unwrap().unwrap();
        assert_eq!(result["text"].as_str().map(str::len), Some(200_000));
        assert_eq!(result["elements"].as_array().map(Vec::len), Some(120));
    }

    /// 客户端报告的失败沿 result 路由透传给等待方。
    #[tokio::test]
    async fn propagates_a_client_reported_failure() {
        let (broker, emitted) = broker_with_log(1);
        // Capture the outcome before posting: the rejection lands while the
        // POST is still in flight, and an unattached handler surfaces as an
        // unhandled one.
        let inflight = start_inflight(&broker, "browser.click").await;
        let request_id = first_request_id(&emitted);

        let (status, body) = post_json(
            app(Arc::clone(&broker)),
            "/api/browser-control/result",
            json!({ "requestId": request_id, "ok": false, "error": "No element matches #nope" })
                .to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({ "matched": true }));

        assert_eq!(
            inflight.await.unwrap().unwrap_err().message,
            "No element matches #nope"
        );
    }

    /// 超时后才到达的回传返回 `matched: false`，而非错误。
    #[tokio::test]
    async fn reports_matched_false_for_a_response_that_arrived_after_the_timeout() {
        let (broker, _) = broker_with_log(1);
        let (status, body) = post_json(
            app(broker),
            "/api/browser-control/result",
            json!({ "requestId": "expired", "ok": true, "data": {} }).to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({ "matched": false }));
    }

    /// 缺 requestId 的请求体返回 400。
    #[tokio::test]
    async fn rejects_a_body_with_no_request_id() {
        let (broker, _) = broker_with_log(1);
        let (status, body) = post_json(
            app(broker),
            "/api/browser-control/result",
            json!({ "ok": true }).to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body, json!({ "error": "requestId is required" }));
    }

    /// 非对象请求体（如纯字符串）返回 400。
    #[tokio::test]
    async fn rejects_a_body_that_is_not_an_object() {
        let (broker, _) = broker_with_log(1);
        let (status, body) = post_json(
            app(broker),
            "/api/browser-control/result",
            "\"just-a-string\"".to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body, json!({ "error": "A JSON body is required" }));
    }

    /// claim 路由先到先得，后续认领被拒绝且不影响最终结果回传。
    #[tokio::test]
    async fn grants_the_first_claim_over_the_claim_route_and_refuses_the_rest() {
        let broker = BrowserControlBroker::with_id_factory(|_| 2, || "req-1".to_string());
        let inflight = start_inflight(&broker, "browser.click").await;
        let router = app(Arc::clone(&broker));

        let (status, body) = post_json(
            router.clone(),
            "/api/browser-control/claim",
            json!({ "requestId": "req-1" }).to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({ "granted": true }));

        let (status, body) = post_json(
            router,
            "/api/browser-control/claim",
            json!({ "requestId": "req-1" }).to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({ "granted": false }));

        let (status, body) = post_json(
            app(Arc::clone(&broker)),
            "/api/browser-control/result",
            json!({ "requestId": "req-1", "ok": true, "data": { "clicked": true } }).to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({ "matched": true }));
        assert_eq!(inflight.await.unwrap().unwrap(), json!({ "clicked": true }));
    }

    /// claim 路由要求 requestId。
    #[tokio::test]
    async fn claim_route_requires_a_request_id() {
        let (broker, _) = broker_with_log(1);
        let (status, body) = post_json(
            app(broker),
            "/api/browser-control/claim",
            json!({}).to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body, json!({ "error": "requestId is required" }));
    }

    /// claim 路由把非对象体当作缺 requestId 处理（无独立的 400 分支）。
    #[tokio::test]
    async fn claim_route_treats_a_non_object_body_as_a_missing_request_id() {
        // routes.js reads `req.body?.requestId`, so a string body never has
        // one — unlike the result route, this route has no "JSON body
        // required" branch.
        let (broker, _) = broker_with_log(1);
        let (status, body) = post_json(
            app(broker),
            "/api/browser-control/claim",
            "\"just-a-string\"".to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body, json!({ "error": "requestId is required" }));
    }

    /// claim 路由拒绝超过 4 KB 的请求体（413）。
    #[tokio::test]
    async fn claim_route_rejects_a_body_over_the_4kb_limit() {
        let (broker, _) = broker_with_log(1);
        let big = format!("{{\"requestId\":\"{}\"}}", "x".repeat(8 * 1024));
        let (status, _) = post_json(app(broker), "/api/browser-control/claim", big).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    }

    /// result 路由拒绝超过 2 MB 的请求体（413）。
    #[tokio::test]
    async fn result_route_rejects_a_body_over_the_2mb_limit() {
        let (broker, _) = broker_with_log(1);
        let big = format!(
            "{{\"requestId\":\"x\",\"padding\":\"{}\"}}",
            "x".repeat(2 * 1024 * 1024)
        );
        let (status, _) = post_json(app(broker), "/api/browser-control/result", big).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    }
}
