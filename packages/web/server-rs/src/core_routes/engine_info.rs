//! `GET /api/opencode/health` and `GET /api/opencode/version` from
//! `opencode/routes.js`: proxy the engine's `GET /global/health` and surface
//! its verdict (health) or version field.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::context::RouterContext;

struct EngineHealthProbe {
    status: u16,
    body: Option<serde_json::Value>,
}

impl EngineHealthProbe {
    fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

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

fn passthrough_status(status: u16) -> StatusCode {
    StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY)
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core_routes::router;
    use crate::core_routes::tests::{json_response, temp_dir, test_ctx};
    use crate::engine::EngineState;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
