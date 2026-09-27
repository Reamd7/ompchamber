//! `GET /api/opencode/health` and `GET /api/opencode/version` from
//! `opencode/routes.js`: proxy the engine's `GET /global/health` and surface
//! its verdict (health) or version field.
//!
//! 中文说明：引擎健康与版本信息路由。二者都先探测引擎的
//! `GET /global/health`：健康路由透传引擎的 `healthy` 判定，版本路由读取
//! `version` 字段并去掉前导 `v`；引擎完全不可达时回退到快照中记录的
//! lastError，保证错误信息与 JS 端 fetch 失败时一致。

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::context::RouterContext;

/// 引擎 `GET /global/health` 的一次探测结果：HTTP 状态码与可选的 JSON 响应体。
struct EngineHealthProbe {
    /// 引擎返回的 HTTP 状态码。
    status: u16,
    /// 解析成功的 JSON 响应体；响应非 JSON 或读取失败时为 None。
    body: Option<serde_json::Value>,
}

/// 探测结果的判定与错误文案推导。
impl EngineHealthProbe {
    /// 2xx 视为探测成功（引擎可寻址且接受了请求）。
    fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// 生成错误文案：优先取响应体的 `error` 字段，其次状态码的标准
    /// reason 短语，最后回退到调用方提供的默认文案。
    /// `health?.error || statusText || <fallback>` — reqwest does not preserve
    /// non-canonical reason phrases, so `canonical_reason` stands in.
    fn error_message(&self, fallback: &str) -> String {
        self.body
            .as_ref()
            .and_then(|body| body.get("error"))
            .and_then(|error| error.as_str())
            .map(str::to_string)
            .filter(|message| !message.is_empty())
            .or_else(|| {
                StatusCode::from_u16(self.status)
                    .ok()
                    .and_then(|s| s.canonical_reason().map(str::to_string))
            })
            .unwrap_or_else(|| fallback.to_string())
    }
}

/// 引擎完全不可达时的错误文案：优先取引擎快照中的 lastError（非空才用），
/// 否则回退默认文案，保持与 JS fetch 失败同样的信息量。
/// Engine not addressable at all: fall back to the recorded engine error so
/// the message carries the same information the JS fetch failure would.
fn engine_unreachable_message(ctx: &RouterContext, fallback: &str) -> String {
    ctx.engine
        .snapshot()
        .get("lastError")
        .and_then(|value| value.as_str())
        .map(str::to_string)
        .filter(|message| !message.is_empty())
        .unwrap_or_else(|| fallback.to_string())
}
/// 请求引擎 `GET /global/health`：拼接 base URL，附带 accept 头与可选的
/// authorization 头。引擎端口未分配或网络失败时返回 Err（错误字符串），
/// 其余情况把状态码与尽力解析的响应体包成探测结果。
async fn fetch_engine_health(ctx: &RouterContext) -> Result<EngineHealthProbe, String> {
    let base = ctx
        .engine
        .base_url()
        .ok_or_else(|| "OpenCode port is not available".to_string())?;
    let url = format!("{}/global/health", base.trim_end_matches('/'));
    let mut request = ctx
        .engine
        .http()
        .get(&url)
        .header("accept", "application/json");
    if let Some(auth) = ctx.engine.auth_header() {
        request = request.header("authorization", auth);
    }
    let response = request.send().await.map_err(|error| error.to_string())?;
    let status = response.status().as_u16();
    let body = response.json::<serde_json::Value>().await.ok();
    Ok(EngineHealthProbe { status, body })
}

/// u16 状态码转 `StatusCode`；非法值（引擎不应返回）回退 502。
fn passthrough_status(status: u16) -> StatusCode {
    StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY)
}

/// 健康路由：探测成功（2xx）时透传引擎 `healthy` 布尔；非 2xx 时透传
/// 引擎状态码与 error 文案；引擎不可达时返回 503 并带上 lastError 信息。
/// `GET /api/opencode/health`.
pub(crate) async fn opencode_health(State(ctx): State<RouterContext>) -> Response {
    match fetch_engine_health(&ctx).await {
        Ok(probe) if probe.ok() => {
            let healthy = probe
                .body
                .as_ref()
                .and_then(|body| body.get("healthy"))
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            (
                StatusCode::OK,
                Json(serde_json::json!({ "healthy": healthy })),
            )
                .into_response()
        }
        Ok(probe) => (
            passthrough_status(probe.status),
            Json(serde_json::json!({
                "healthy": false,
                "error": probe.error_message("OpenCode health check failed"),
            })),
        )
            .into_response(),
        Err(error) => {
            let message = if error.is_empty() {
                engine_unreachable_message(&ctx, "OpenCode health check failed")
            } else {
                error
            };
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({ "healthy": false, "error": message })),
            )
                .into_response()
        }
    }
}

/// 版本路由：探测成功时返回引擎 `version` 字段（去掉前导 `v`，缺失为
/// null）；非 2xx 透传状态码与 error；不可达时 500 + lastError。
/// `GET /api/opencode/version` — engine version with the leading `v` stripped.
pub(crate) async fn opencode_version(State(ctx): State<RouterContext>) -> Response {
    match fetch_engine_health(&ctx).await {
        Ok(probe) if probe.ok() => {
            let version = probe
                .body
                .as_ref()
                .and_then(|body| body.get("version"))
                .and_then(|value| value.as_str())
                .map(|version| version.strip_prefix('v').unwrap_or(version).to_string());
            (
                StatusCode::OK,
                Json(serde_json::json!({ "version": version })),
            )
                .into_response()
        }
        Ok(probe) => (
            passthrough_status(probe.status),
            Json(serde_json::json!({
                "version": serde_json::Value::Null,
                "error": probe.error_message("Failed to read engine version"),
            })),
        )
            .into_response(),
        Err(error) => {
            let message = if error.is_empty() {
                engine_unreachable_message(&ctx, "Failed to read engine version")
            } else {
                error
            };
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "version": serde_json::Value::Null, "error": message })),
            )
                .into_response()
        }
    }
}

/// 单元测试：借助本地假引擎覆盖健康/版本路由的成功、非 2xx 透传与
/// 引擎不可达三类路径，以及鉴权头的携带。
#[cfg(test)]
mod tests {
    use super::*;
    use crate::core_routes::router;
    use crate::core_routes::tests::{json_response, temp_dir, test_ctx};
    use crate::engine::EngineState;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// 启动一个最小 HTTP 假引擎：对任意请求都回复给定的状态行与 JSON 载荷，
    /// 返回其 base URL。
    /// Minimal fake engine: answers every request with `payload` and `status`.
    async fn spawn_fake_engine(status_line: &'static str, payload: &'static str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake engine");
        let addr = listener.local_addr().expect("fake engine addr");
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let payload = payload.to_string();
                let status_line = status_line.to_string();
                tokio::spawn(async move {
                    let mut buffer = [0u8; 8192];
                    let _ = socket.read(&mut buffer).await;
                    let response = format!(
                        "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        payload.len(),
                        payload
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        format!("http://{addr}")
    }

    /// 验证：引擎报告 healthy:true 时路由原样透传该判定（200）。
    #[tokio::test]
    async fn opencode_health_reports_engine_verdict() {
        let base = spawn_fake_engine("200 OK", r#"{"healthy":true,"version":"v1.2.3"}"#).await;
        let ctx = test_ctx(temp_dir("engine-health"), EngineState::external(base, None));
        let (status, body) = json_response(
            router(ctx),
            HttpRequest::builder()
                .uri("/api/opencode/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, serde_json::json!({ "healthy": true }));
    }

    /// 验证：引擎自身判定 unhealthy 时路由仍返回 200 与 healthy:false。
    #[tokio::test]
    async fn opencode_health_reports_unhealthy_engine_verdict() {
        let base = spawn_fake_engine("200 OK", r#"{"healthy":false}"#).await;
        let ctx = test_ctx(
            temp_dir("engine-unhealthy"),
            EngineState::external(base, None),
        );
        let (_, body) = json_response(
            router(ctx),
            HttpRequest::builder()
                .uri("/api/opencode/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(body, serde_json::json!({ "healthy": false }));
    }

    /// 验证：引擎返回 401 时路由透传该状态码，并携带引擎的 error 字段。
    #[tokio::test]
    async fn opencode_health_passes_engine_error_status_through() {
        let base = spawn_fake_engine("401 Unauthorized", r#"{"error":"unauthorized"}"#).await;
        let ctx = test_ctx(temp_dir("engine-401"), EngineState::external(base, None));
        let (status, body) = json_response(
            router(ctx),
            HttpRequest::builder()
                .uri("/api/opencode/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["healthy"], false);
        assert_eq!(body["error"], "unauthorized");
    }

    /// 验证：引擎端口无监听时健康路由返回 503、healthy:false 且带 error 文案。
    #[tokio::test]
    async fn opencode_health_unreachable_engine_is_503_with_error() {
        // Bind then drop a port so nothing listens on it.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        drop(listener);
        let ctx = test_ctx(
            temp_dir("engine-dead"),
            EngineState::external(format!("http://{addr}"), None),
        );
        let (status, body) = json_response(
            router(ctx),
            HttpRequest::builder()
                .uri("/api/opencode/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["healthy"], false);
        assert!(body["error"].is_string());
    }

    /// 验证：配置密码后请求携带 Basic 鉴权头（只断言 scheme，不泄露密钥），
    /// 且健康探测仍成功。
    #[tokio::test]
    async fn engine_sends_authorization_header_when_password_set() {
        let base = spawn_fake_engine("200 OK", r#"{"healthy":true}"#).await;
        let engine = EngineState::external(base, Some("secret-password".to_string()));
        // The auth header must exist and be Basic — its value contains the
        // secret, so only assert the scheme here (never log the value).
        let header = engine.auth_header().expect("auth header");
        assert!(header.starts_with("Basic "), "auth header must be Basic");
        let ctx = test_ctx(temp_dir("engine-auth"), engine);
        let (status, body) = json_response(
            router(ctx),
            HttpRequest::builder()
                .uri("/api/opencode/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["healthy"], true);
    }

    /// 验证：引擎响应缺少 version 字段时版本路由返回 null 而非报错。
    #[tokio::test]
    async fn opencode_version_without_version_field_is_null() {
        let base = spawn_fake_engine("200 OK", r#"{"healthy":true}"#).await;
        let ctx = test_ctx(
            temp_dir("engine-version-null"),
            EngineState::external(base, None),
        );
        let (_, body) = json_response(
            router(ctx),
            HttpRequest::builder()
                .uri("/api/opencode/version")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(body, serde_json::json!({ "version": null }));
    }

    /// 验证：引擎不可达时版本路由返回 500、version:null 与 error 文案。
    #[tokio::test]
    async fn opencode_version_unreachable_engine_is_500_with_null_version() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        drop(listener);
        let ctx = test_ctx(
            temp_dir("engine-version-dead"),
            EngineState::external(format!("http://{addr}"), None),
        );
        let (status, body) = json_response(
            router(ctx),
            HttpRequest::builder()
                .uri("/api/opencode/version")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(body["version"].is_null());
        assert!(body["error"].is_string());
    }
}
